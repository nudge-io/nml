use crate::error::NmlError;
use crate::span::Span;
use serde::Serialize;

/// A money value stored as integer minor units with ISO 4217 currency code.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Money {
    /// Amount in minor units (e.g., cents for USD).
    pub amount: i64,
    /// ISO 4217 currency code (e.g., "USD").
    pub currency: String,
    /// Number of decimal places for this currency (from ISO 4217).
    pub exponent: u8,
}

impl Money {
    pub fn format_display(&self) -> String {
        format!("{} {}", self.to_number(), self.currency)
    }

    /// The exact decimal amount (RFC 0016 §1.8): minor units at the
    /// currency exponent *are* a `(coeff, scale)` member of the shared
    /// numeric core, so display and any exact consumer share one
    /// formatter with `number`.
    pub fn to_number(&self) -> crate::decimal::Number {
        crate::decimal::Number::try_new(self.amount as i128, self.exponent as i16)
            .expect("i64 minor units at an ISO 4217 exponent are always a valid Number")
    }
}

/// Why a money literal failed to parse (RFC 0009 D13) — fully structured:
/// message, stable code, and the ISO-4217 did-you-mean all derive from the
/// payload, never from message text.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum MoneyErrorKind {
    /// Not in the ISO 4217 table. `code_span` is the currency token's own
    /// sub-span, so the did-you-mean fix is machine-applicable.
    UnknownCurrency { code: String, code_span: Span },
    /// More fractional digits than the currency's minor unit allows.
    Precision {
        currency: String,
        allowed: u8,
        got: usize,
        raw: String,
    },
    /// The amount is not a well-formed decimal.
    InvalidAmount { raw: String },
    /// The scaled minor-unit amount exceeds `i64` (or the amount falls
    /// outside the exact decimal domain) — money is exact by design,
    /// never floated.
    OutOfRange { raw: String },
}

impl MoneyErrorKind {
    /// The human-facing message, derived from the payload.
    pub fn message(&self) -> String {
        use crate::error::echo;
        match self {
            MoneyErrorKind::UnknownCurrency { code, .. } => {
                format!("unknown currency code: {}", echo(code))
            }
            MoneyErrorKind::Precision {
                currency,
                allowed,
                got,
                raw,
            } => format!(
                "{currency} has {allowed} decimal places, but got {got} in \"{}\"",
                echo(raw)
            ),
            MoneyErrorKind::InvalidAmount { raw } => format!("invalid number: \"{}\"", echo(raw)),
            MoneyErrorKind::OutOfRange { raw } => {
                format!("amount out of range: \"{}\"", echo(raw))
            }
        }
    }

    /// The stable code — total: money errors are always coded.
    pub fn code(&self) -> crate::diagnostic::Code {
        use crate::diagnostic::codes;
        match self {
            MoneyErrorKind::UnknownCurrency { .. } => codes::UNKNOWN_CURRENCY,
            MoneyErrorKind::Precision { .. } => codes::MONEY_PRECISION,
            MoneyErrorKind::InvalidAmount { .. } => codes::INVALID_MONEY,
            MoneyErrorKind::OutOfRange { .. } => codes::AMOUNT_OUT_OF_RANGE,
        }
    }
}

/// Parse a money literal like "19.99 USD" into a `Money` value.
///
/// **Span contract**: `span` must cover exactly `amount ++ separator ++
/// currency` in the source, with `amount_str` byte-contiguous at
/// `span.start` and `currency` at `span.end` — the machine-applicable
/// sub-spans (unknown-currency did-you-mean, trailing-dot fix) are
/// derived arithmetically from it. The CST decoder honors this by
/// intercepting trailing-dot amounts on the number token's real range
/// BEFORE calling here (gap spellings like `- 1299. JPY` would otherwise
/// break the arithmetic — implementation-review finding).
pub fn parse_money(amount_str: &str, currency: &str, span: Span) -> Result<Money, NmlError> {
    let exponent = match currency_exponent(currency) {
        Some(e) => e,
        None => {
            return Err(NmlError::Money {
                kind: MoneyErrorKind::UnknownCurrency {
                    code: crate::error::echo_capture(currency),
                    // The currency is the literal's trailing token, so its
                    // own sub-span is the last `currency.len()` bytes
                    // (ASCII) — captured structurally for the did-you-mean.
                    code_span: Span::new(span.end.saturating_sub(currency.len()), span.end),
                },
                span,
            });
        }
    };

    let amount = parse_minor_units(amount_str, exponent, currency, span)?;

    Ok(Money {
        amount,
        currency: currency.to_string(),
        exponent,
    })
}

/// Parse the amount through the shared exact-decimal core and rescale to
/// i64 minor units (RFC 0016 §1.8). The rescale runs in i128 (≤ 34-digit
/// coefficient × 10^4 max ISO exponent ≈ 10^38, inside i128) with a
/// checked i64 narrow.
///
/// Error-band mapping, pinned: amount-domain failures never surface
/// syntax-band codes — decimal-core rejections re-wrap as
/// `MoneyErrorKind` (malformed → `InvalidAmount` NML3000, out-of-domain
/// → `OutOfRange` NML3003). The one carve-out is the trailing-dot
/// malformation (`1299. JPY`): a form defect of the literal token, not
/// of the amount, so it surfaces as NML0013 with the same drop-the-dot
/// suggestion as bare `1299.`.
fn parse_minor_units(
    amount_str: &str,
    exponent: u8,
    currency: &str,
    span: Span,
) -> Result<i64, NmlError> {
    // `span` covers the whole literal (`1299. JPY`); the amount is its
    // ASCII prefix. The trailing-dot suggestion deletes the literal's
    // final byte, so it must anchor on the AMOUNT sub-span — anchoring on
    // `span` would delete the last byte of the currency code instead.
    let amount_span = Span::new(span.start, span.start + amount_str.len());
    let n = crate::decimal::Number::parse_literal(amount_str).map_err(|e| match e {
        crate::decimal::NumberError::TrailingDot => NmlError::syntax(
            crate::error::ParseErrorKind::NumberTrailingDot {
                raw: crate::error::echo_capture(amount_str),
            },
            amount_span,
        ),
        // A misplaced separator is a form defect of the literal token —
        // the same literal-layer carve-out as the trailing dot, anchored
        // on the amount sub-span so the strip fix rewrites the amount and
        // never touches the currency code.
        crate::decimal::NumberError::BadSeparator => NmlError::syntax(
            crate::error::ParseErrorKind::NumberBadSeparator {
                raw: crate::error::echo_capture(amount_str),
                stripped: crate::error::strip_separators_fix(amount_str),
            },
            amount_span,
        ),
        crate::decimal::NumberError::Malformed => NmlError::Money {
            kind: MoneyErrorKind::InvalidAmount {
                raw: crate::error::echo_capture(amount_str),
            },
            span,
        },
        // Explicit arms, no wildcard: a wildcard would silently absorb
        // future variants into the "amount out of range" voice. The
        // parse core emits exactly these three:
        crate::decimal::NumberError::Range(
            crate::decimal::NumberRangeIssue::TooManyDigits { .. }
            | crate::decimal::NumberRangeIssue::TooLarge
            | crate::decimal::NumberRangeIssue::TooSmall,
        ) => NmlError::Money {
            kind: MoneyErrorKind::OutOfRange {
                raw: crate::error::echo_capture(amount_str),
            },
            span,
        },
        // try_new-only kinds — unreachable from `parse_literal` (raw
        // pairs never come from a parse). Mapped to the same range error
        // rather than `unreachable!()` so a future defect degrades to a
        // wrong-but-safe message, never a parse-path panic.
        crate::decimal::NumberError::Range(
            crate::decimal::NumberRangeIssue::CoefficientTooWide { .. }
            | crate::decimal::NumberRangeIssue::ScaleOutOfRange { .. }
            | crate::decimal::NumberRangeIssue::NegativeScaleZero { .. },
        ) => NmlError::Money {
            kind: MoneyErrorKind::OutOfRange {
                raw: crate::error::echo_capture(amount_str),
            },
            span,
        },
    })?;

    // The written fraction width against the currency's minor-unit rule.
    // `got` reports what the author WROTE (the parse clamps scale at 34
    // significant digits, and an honest count beats a clamped one in the
    // message); the condition uses the stored scale, which the clamp can
    // only have reduced — never below a violating width's threshold,
    // since ISO exponents cap at 4 and the clamp floor for fractions
    // with a violating written width stays above it.
    if n.scale() > exponent as i16 {
        // Digit count, not byte count: `19.9_9` wrote 2 fraction digits,
        // and the message must say so (separators are spelling).
        let written = amount_str
            .split_once('.')
            .map(|(_, frac)| frac.bytes().filter(|b| b.is_ascii_digit()).count())
            .unwrap_or(0);
        return Err(NmlError::Money {
            kind: MoneyErrorKind::Precision {
                currency: crate::error::echo_capture(currency),
                allowed: exponent,
                got: written.max(n.scale() as usize),
                raw: crate::error::echo_capture(amount_str),
            },
            span,
        });
    }

    // minor units = coeff × 10^(exponent − scale), exact in i128.
    let shift = (exponent as i32) - (n.scale() as i32);
    let out_of_range = || NmlError::Money {
        kind: MoneyErrorKind::OutOfRange {
            raw: crate::error::echo_capture(amount_str),
        },
        span,
    };
    10i128
        .checked_pow(shift as u32)
        .and_then(|m| n.coeff().checked_mul(m))
        .and_then(|units| i64::try_from(units).ok())
        .ok_or_else(out_of_range)
}

// ISO 4217 codes grouped by exponent — the single source for both exponent
// lookup and the unknown-currency did-you-mean candidates (RFC 0008), so the
// two can never drift.
const EXPONENT_0: &[&str] = &[
    "BIF", "CLP", "DJF", "GNF", "ISK", "JPY", "KMF", "KRW", "PYG", "RWF", "UGX", "UYI", "VND",
    "VUV", "XAF", "XOF", "XPF",
];
const EXPONENT_2: &[&str] = &[
    "AED", "AFN", "ALL", "AMD", "ANG", "AOA", "ARS", "AUD", "AWG", "AZN", "BAM", "BBD", "BDT",
    "BGN", "BMD", "BND", "BOB", "BRL", "BSD", "BTN", "BWP", "BYN", "BZD", "CAD", "CDF", "CHF",
    "CNY", "COP", "CRC", "CUP", "CVE", "CZK", "DKK", "DOP", "DZD", "EGP", "ERN", "ETB", "EUR",
    "FJD", "FKP", "GBP", "GEL", "GHS", "GIP", "GMD", "GTQ", "GYD", "HKD", "HNL", "HTG", "HUF",
    "IDR", "ILS", "INR", "IRR", "JMD", "KES", "KGS", "KHR", "KYD", "KZT", "LAK", "LBP", "LKR",
    "LRD", "LSL", "MAD", "MDL", "MGA", "MKD", "MMK", "MNT", "MOP", "MRU", "MUR", "MVR", "MWK",
    "MXN", "MYR", "MZN", "NAD", "NGN", "NIO", "NOK", "NPR", "NZD", "PAB", "PEN", "PGK", "PHP",
    "PKR", "PLN", "QAR", "RON", "RSD", "RUB", "SAR", "SBD", "SCR", "SDG", "SEK", "SGD", "SHP",
    "SLE", "SOS", "SRD", "SSP", "STN", "SYP", "SZL", "THB", "TJS", "TMT", "TOP", "TRY", "TTD",
    "TWD", "TZS", "UAH", "USD", "UYU", "UZS", "VES", "WST", "XCD", "YER", "ZAR", "ZMW", "ZWL",
];
const EXPONENT_3: &[&str] = &["BHD", "IQD", "JOD", "KWD", "LYD", "OMR", "TND"];
const EXPONENT_4: &[&str] = &["CLF", "UYW"];

const CURRENCY_BANDS: &[(&[&str], u8)] = &[
    (EXPONENT_0, 0),
    (EXPONENT_2, 2),
    (EXPONENT_3, 3),
    (EXPONENT_4, 4),
];

/// The currency-code *shape* — exactly three uppercase ASCII letters —
/// recognized structurally, before ISO 4217 validity is judged
/// ([`parse_money`]'s job). The one classification predicate for a
/// number's trailing identifier on the money side: disjoint by
/// construction from duration units (one or two lowercase letters), so
/// the value decoder needs no lookahead to route between them (RFC 0017
/// §1).
pub(crate) fn is_currency_code(text: &str) -> bool {
    text.len() == 3 && text.bytes().all(|b| b.is_ascii_uppercase())
}

/// Every known ISO 4217 code — the unknown-currency suggestion candidates.
pub(crate) fn currency_codes() -> impl Iterator<Item = &'static str> {
    CURRENCY_BANDS
        .iter()
        .flat_map(|(codes, _)| codes.iter().copied())
}

/// Returns the ISO 4217 exponent (minor unit count) for a currency code.
pub fn currency_exponent(code: &str) -> Option<u8> {
    CURRENCY_BANDS
        .iter()
        .find_map(|(codes, exponent)| codes.contains(&code).then_some(*exponent))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span() -> Span {
        Span::new(0, 0)
    }

    #[test]
    fn unknown_currency_captures_code_and_subspan_for_suggestion() {
        // Literal "19.99 USE" occupying bytes 10..19: the captured sub-span
        // must cover exactly the trailing code, and the diagnostic bridge
        // must turn it into a machine-applicable ISO-4217 did-you-mean.
        let err = parse_money("19.99", "USE", Span::new(10, 19)).unwrap_err();
        let NmlError::Money {
            kind: MoneyErrorKind::UnknownCurrency { code, code_span },
            ..
        } = &err
        else {
            panic!("expected structured currency capture: {err:?}");
        };
        assert_eq!(code, "USE");
        assert_eq!((code_span.start, code_span.end), (16, 19));
        let diag = err.to_diagnostic();
        assert_eq!(diag.code, Some(crate::diagnostic::codes::UNKNOWN_CURRENCY));
        let sug = diag
            .suggestions
            .first()
            .cloned()
            .expect("ISO-4217 did-you-mean");
        assert_eq!(sug.replacement, "USD");
        assert_eq!((sug.span.start, sug.span.end), (16, 19));
    }

    #[test]
    fn test_parse_usd() {
        let m = parse_money("19.99", "USD", span()).unwrap();
        assert_eq!(m.amount, 1999);
        assert_eq!(m.currency, "USD");
        assert_eq!(m.exponent, 2);
    }

    #[test]
    fn test_parse_jpy() {
        let m = parse_money("1299", "JPY", span()).unwrap();
        assert_eq!(m.amount, 1299);
        assert_eq!(m.exponent, 0);
    }

    #[test]
    fn test_parse_bhd() {
        let m = parse_money("5.125", "BHD", span()).unwrap();
        assert_eq!(m.amount, 5125);
        assert_eq!(m.exponent, 3);
    }

    #[test]
    fn test_too_many_decimals() {
        let result = parse_money("19.999", "USD", span());
        assert!(result.is_err());
    }

    #[test]
    fn test_unknown_currency() {
        let result = parse_money("10.00", "XYZ", span());
        assert!(result.is_err());
    }

    #[test]
    fn test_display() {
        let m = parse_money("19.99", "USD", span()).unwrap();
        assert_eq!(m.format_display(), "19.99 USD");

        let m = parse_money("1299", "JPY", span()).unwrap();
        assert_eq!(m.format_display(), "1299 JPY");
    }

    #[test]
    fn test_partial_fraction() {
        let m = parse_money("5.5", "USD", span()).unwrap();
        assert_eq!(m.amount, 550);
        assert_eq!(m.format_display(), "5.50 USD");
    }

    #[test]
    fn test_negative_amount() {
        let m = parse_money("-19.99", "USD", span()).unwrap();
        assert_eq!(m.amount, -1999);
        assert_eq!(m.exponent, 2);
        assert_eq!(m.format_display(), "-19.99 USD");
    }

    #[test]
    fn test_whole_number_usd() {
        let m = parse_money("100", "USD", span()).unwrap();
        assert_eq!(m.amount, 10000);
        assert_eq!(m.format_display(), "100.00 USD");
    }

    #[test]
    fn test_zero_amount() {
        let m = parse_money("0", "USD", span()).unwrap();
        assert_eq!(m.amount, 0);
        assert_eq!(m.format_display(), "0.00 USD");
    }

    #[test]
    fn test_zero_with_decimals() {
        let m = parse_money("0.00", "USD", span()).unwrap();
        assert_eq!(m.amount, 0);
        assert_eq!(m.format_display(), "0.00 USD");
    }

    #[test]
    fn test_exponent_4_currency() {
        let m = parse_money("1.2345", "CLF", span()).unwrap();
        assert_eq!(m.amount, 12345);
        assert_eq!(m.exponent, 4);
        assert_eq!(m.format_display(), "1.2345 CLF");
    }

    #[test]
    fn test_exponent_4_too_many_decimals() {
        let result = parse_money("1.23456", "CLF", span());
        assert!(result.is_err());
    }

    #[test]
    fn test_exponent_4_partial_fraction() {
        let m = parse_money("1.5", "CLF", span()).unwrap();
        assert_eq!(m.amount, 15000);
        assert_eq!(m.format_display(), "1.5000 CLF");
    }

    #[test]
    fn test_large_amount() {
        let m = parse_money("999999999.99", "USD", span()).unwrap();
        assert_eq!(m.amount, 99999999999);
        assert_eq!(m.format_display(), "999999999.99 USD");
    }

    #[test]
    fn test_one_cent() {
        let m = parse_money("0.01", "USD", span()).unwrap();
        assert_eq!(m.amount, 1);
        assert_eq!(m.format_display(), "0.01 USD");
    }

    #[test]
    fn test_negative_zero() {
        let m = parse_money("-0.00", "USD", span()).unwrap();
        assert_eq!(m.amount, 0);
    }

    #[test]
    fn test_overflow_rejected() {
        // i64::MAX minor units is ~9.2e18; this whole amount overflows on
        // scaling by 100 and must produce an error, not a wrapped value.
        let result = parse_money("9223372036854775807", "USD", span());
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("out of range"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_max_representable_succeeds() {
        // i64::MAX is 9223372036854775807 => max USD whole+frac amount.
        let m = parse_money("92233720368547758.07", "USD", span()).unwrap();
        assert_eq!(m.amount, i64::MAX);
    }

    #[test]
    fn test_bhd_partial() {
        let m = parse_money("5.12", "BHD", span()).unwrap();
        assert_eq!(m.amount, 5120);
        assert_eq!(m.format_display(), "5.120 BHD");
    }

    #[test]
    fn test_jpy_rejects_decimals() {
        let result = parse_money("100.5", "JPY", span());
        assert!(result.is_err());
    }

    #[test]
    fn test_format_display_negative_fraction() {
        let m = Money {
            amount: -50,
            currency: "USD".to_string(),
            exponent: 2,
        };
        assert_eq!(m.format_display(), "-0.50 USD");
    }

    #[test]
    fn test_currency_exponent_lookup() {
        assert_eq!(currency_exponent("USD"), Some(2));
        assert_eq!(currency_exponent("EUR"), Some(2));
        assert_eq!(currency_exponent("GBP"), Some(2));
        assert_eq!(currency_exponent("JPY"), Some(0));
        assert_eq!(currency_exponent("KRW"), Some(0));
        assert_eq!(currency_exponent("BHD"), Some(3));
        assert_eq!(currency_exponent("KWD"), Some(3));
        assert_eq!(currency_exponent("CLF"), Some(4));
        assert_eq!(currency_exponent("FAKE"), None);
        assert_eq!(currency_exponent(""), None);
    }

    // --- RFC 0016 §1.8: the decimal-core rebase boundaries ---

    #[test]
    fn i64_min_minor_units_now_valid() {
        // Exactly i64::MIN cents: the old abs-then-negate path rejected
        // it; the signed i128 rescale accepts it (documented widening).
        let m = parse_money("-92233720368547758.08", "USD", Span::empty(0)).unwrap();
        assert_eq!(m.amount, i64::MIN);
        assert_eq!(m.format_display(), "-92233720368547758.08 USD");
    }

    #[test]
    fn twenty_digit_amount_is_out_of_range_not_invalid() {
        // Boundary shift, documented: dies as NML3003 OutOfRange (the
        // amount parses exactly but exceeds i64 minor units), where the
        // old whole_str.parse::<i64>() failed as NML3000.
        let err = parse_money("99999999999999999999", "JPY", Span::empty(0)).unwrap_err();
        match err {
            NmlError::Money {
                kind: MoneyErrorKind::OutOfRange { .. },
                ..
            } => {}
            other => panic!("expected OutOfRange, got {other:?}"),
        }
    }

    #[test]
    fn trailing_dot_money_is_a_literal_layer_rejection() {
        // `1299. JPY`: a form defect of the token — NML0013 with the
        // drop-the-dot suggestion, never a money-band code. The literal
        // spans bytes 0..9 (`1299. JPY`); the amount is bytes 0..5, so
        // the machine fix must delete byte 4 (the dot) — NOT the last
        // byte of the currency code.
        let err = parse_money("1299.", "JPY", Span::new(0, 9)).unwrap_err();
        match &err {
            NmlError::Syntax {
                kind: kind @ crate::error::ParseErrorKind::NumberTrailingDot { .. },
                span,
            } => {
                let crate::error::Repairs::Fix(replacement, fix_span) = kind.repairs(*span) else {
                    panic!("fix exists");
                };
                assert_eq!(replacement, "");
                assert_eq!(
                    (fix_span.start, fix_span.end),
                    (4, 5),
                    "must target the dot"
                );
            }
            other => panic!("expected NumberTrailingDot, got {other:?}"),
        }
    }

    #[test]
    fn to_number_is_the_exact_amount() {
        let m = parse_money("29.99", "USD", Span::empty(0)).unwrap();
        assert_eq!(m.to_number(), crate::num!(29.99));
        assert_eq!(m.to_number().to_string(), "29.99");
        let y = parse_money("1299", "JPY", Span::empty(0)).unwrap();
        assert_eq!(y.to_number(), crate::decimal::Number::from(1299));
        assert_eq!(y.format_display(), "1299 JPY");
        // Negative sub-unit amounts keep the old display exactly.
        let neg = parse_money("-0.50", "USD", Span::empty(0)).unwrap();
        assert_eq!(neg.format_display(), "-0.50 USD");
    }
}
