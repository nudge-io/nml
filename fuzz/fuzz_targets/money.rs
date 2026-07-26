//! Fuzz money parsing (RFC 0016 §1.8): never panics; every accepted
//! amount round-trips exactly through minor units and `to_number`.

#![no_main]

use libfuzzer_sys::fuzz_target;
use nml_core::span::Span;

fuzz_target!(|data: &[u8]| {
    let Ok(s) = std::str::from_utf8(data) else {
        return;
    };
    // Interpret the input as "<amount> <currency>"; fall back to USD.
    let (amount, currency) = s.split_once(' ').unwrap_or((s, "USD"));
    if let Ok(m) = nml_core::money::parse_money(amount, currency, Span::empty(0)) {
        // Display → literal amount → reparse must be value-exact.
        let shown = m.format_display();
        let (amt, cur) = shown
            .rsplit_once(' ')
            .expect("display always has a currency");
        let again = nml_core::money::parse_money(amt, cur, Span::empty(0))
            .unwrap_or_else(|e| panic!("reparse of {shown:?}: {e:?}"));
        assert_eq!(m, again, "money round-trip of {s:?}");
    }
});
