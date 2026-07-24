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
        if self.exponent == 0 {
            return format!("{} {}", self.amount, self.currency);
        }
        let divisor = 10i64.pow(self.exponent as u32);
        let whole = self.amount / divisor;
        let frac = (self.amount % divisor).abs();
        let sign = if self.amount < 0 && whole == 0 {
            "-"
        } else {
            ""
        };
        format!(
            "{sign}{whole}.{frac:0>width$} {currency}",
            currency = self.currency,
            width = self.exponent as usize
        )
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
    /// The whole-number part does not parse.
    InvalidAmount { raw: String },
    /// The fractional part does not parse.
    InvalidFraction { raw: String },
    /// The scaled minor-unit amount exceeds `i64` — money is exact by
    /// design, never floated.
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
            MoneyErrorKind::InvalidFraction { raw } => {
                format!("invalid fractional part: \"{}\"", echo(raw))
            }
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
            MoneyErrorKind::InvalidAmount { .. } | MoneyErrorKind::InvalidFraction { .. } => {
                codes::INVALID_MONEY
            }
            MoneyErrorKind::OutOfRange { .. } => codes::AMOUNT_OUT_OF_RANGE,
        }
    }
}

/// Parse a money literal like "19.99 USD" into a `Money` value.
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

fn parse_minor_units(
    amount_str: &str,
    exponent: u8,
    currency: &str,
    span: Span,
) -> Result<i64, NmlError> {
    let negative = amount_str.starts_with('-');
    let abs_str = if negative {
        &amount_str[1..]
    } else {
        amount_str
    };

    let (whole_str, frac_str) = if let Some(dot_pos) = abs_str.find('.') {
        (&abs_str[..dot_pos], &abs_str[dot_pos + 1..])
    } else {
        (abs_str, "")
    };

    if frac_str.len() > exponent as usize {
        return Err(NmlError::Money {
            kind: MoneyErrorKind::Precision {
                currency: crate::error::echo_capture(currency),
                allowed: exponent,
                got: frac_str.len(),
                raw: crate::error::echo_capture(amount_str),
            },
            span,
        });
    }

    let whole: i64 = whole_str.parse().map_err(|_| NmlError::Money {
        kind: MoneyErrorKind::InvalidAmount {
            raw: crate::error::echo_capture(amount_str),
        },
        span,
    })?;

    let frac: i64 = if frac_str.is_empty() {
        0
    } else {
        let padded = format!("{:0<width$}", frac_str, width = exponent as usize);
        padded.parse().map_err(|_| NmlError::Money {
            kind: MoneyErrorKind::InvalidFraction {
                raw: crate::error::echo_capture(frac_str),
            },
            span,
        })?
    };

    let multiplier = 10i64.pow(exponent as u32);
    let abs_amount = whole
        .checked_mul(multiplier)
        .and_then(|scaled| scaled.checked_add(frac))
        .ok_or_else(|| NmlError::Money {
            kind: MoneyErrorKind::OutOfRange {
                raw: crate::error::echo_capture(amount_str),
            },
            span,
        })?;

    Ok(if negative { -abs_amount } else { abs_amount })
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
}
