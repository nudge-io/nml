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
        format!(
            "{}.{:0>width$} {}",
            whole,
            frac,
            self.currency,
            width = self.exponent as usize
        )
    }
}

/// Parse a money literal like "19.99 USD" into a `Money` value.
pub fn parse_money(amount_str: &str, currency: &str, span: Span) -> Result<Money, NmlError> {
    let exponent = match currency_exponent(currency) {
        Some(e) => e,
        None => {
            return Err(NmlError::InvalidMoney {
                message: format!("unknown currency code: {currency}"),
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
        return Err(NmlError::InvalidMoney {
            message: format!(
                "{currency} has {exponent} decimal places, but got {} in \"{amount_str}\"",
                frac_str.len()
            ),
            span,
        });
    }

    let whole: i64 = whole_str.parse().map_err(|_| NmlError::InvalidMoney {
        message: format!("invalid number: \"{amount_str}\""),
        span,
    })?;

    let frac: i64 = if frac_str.is_empty() {
        0
    } else {
        let padded = format!("{:0<width$}", frac_str, width = exponent as usize);
        padded.parse().map_err(|_| NmlError::InvalidMoney {
            message: format!("invalid fractional part: \"{frac_str}\""),
            span,
        })?
    };

    let multiplier = 10i64.pow(exponent as u32);
    let abs_amount = whole * multiplier + frac;

    Ok(if negative { -abs_amount } else { abs_amount })
}

/// Returns the ISO 4217 exponent (minor unit count) for a currency code.
pub fn currency_exponent(code: &str) -> Option<u8> {
    match code {
        // Exponent 0 (no minor unit)
        "BIF" | "CLP" | "DJF" | "GNF" | "ISK" | "JPY" | "KMF" | "KRW" | "PYG" | "RWF"
        | "UGX" | "UYI" | "VND" | "VUV" | "XAF" | "XOF" | "XPF" => Some(0),

        // Exponent 3
        "BHD" | "IQD" | "JOD" | "KWD" | "LYD" | "OMR" | "TND" => Some(3),

        // Exponent 4
        "CLF" | "UYW" => Some(4),

        // Exponent 2 (the vast majority of currencies)
        "AED" | "AFN" | "ALL" | "AMD" | "ANG" | "AOA" | "ARS" | "AUD" | "AWG" | "AZN"
        | "BAM" | "BBD" | "BDT" | "BGN" | "BMD" | "BND" | "BOB" | "BRL" | "BSD" | "BTN"
        | "BWP" | "BYN" | "BZD" | "CAD" | "CDF" | "CHF" | "CNY" | "COP" | "CRC" | "CUP"
        | "CVE" | "CZK" | "DKK" | "DOP" | "DZD" | "EGP" | "ERN" | "ETB" | "EUR" | "FJD"
        | "FKP" | "GBP" | "GEL" | "GHS" | "GIP" | "GMD" | "GTQ" | "GYD" | "HKD" | "HNL"
        | "HTG" | "HUF" | "IDR" | "ILS" | "INR" | "IRR" | "JMD" | "KES" | "KGS" | "KHR"
        | "KYD" | "KZT" | "LAK" | "LBP" | "LKR" | "LRD" | "LSL" | "MAD" | "MDL" | "MGA"
        | "MKD" | "MMK" | "MNT" | "MOP" | "MRU" | "MUR" | "MVR" | "MWK" | "MXN" | "MYR"
        | "MZN" | "NAD" | "NGN" | "NIO" | "NOK" | "NPR" | "NZD" | "PAB" | "PEN" | "PGK"
        | "PHP" | "PKR" | "PLN" | "QAR" | "RON" | "RSD" | "RUB" | "SAR" | "SBD" | "SCR"
        | "SDG" | "SEK" | "SGD" | "SHP" | "SLE" | "SOS" | "SRD" | "SSP" | "STN" | "SYP"
        | "SZL" | "THB" | "TJS" | "TMT" | "TOP" | "TRY" | "TTD" | "TWD" | "TZS" | "UAH"
        | "USD" | "UYU" | "UZS" | "VES" | "WST" | "XCD" | "YER" | "ZAR" | "ZMW" | "ZWL" => {
            Some(2)
        }

        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span() -> Span {
        Span::new(0, 0)
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
}
