//! Fuzz the exact-decimal core (RFC 0016 §2): both grammars must never
//! panic, and every accepted value must satisfy the round-trip, equality,
//! and hashing invariants.

#![no_main]

use libfuzzer_sys::fuzz_target;
use nml_core::decimal::Number;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

fn h(n: &Number) -> u64 {
    let mut s = DefaultHasher::new();
    n.hash(&mut s);
    s.finish()
}

fuzz_target!(|data: &[u8]| {
    let Ok(s) = std::str::from_utf8(data) else {
        return;
    };
    for parse in [Number::parse_literal, Number::parse_coercion] {
        if let Ok(n) = parse(s) {
            // Display output reparses to an Eq-equal value with an equal
            // hash, and comparison is reflexive.
            let d = n.to_string();
            let r = Number::parse_literal(&d).unwrap_or_else(|e| panic!("reparse of {d:?}: {e:?}"));
            assert_eq!(n, r, "round-trip of {s:?}");
            assert_eq!(h(&n), h(&r), "hash round-trip of {s:?}");
            assert_eq!(n.cmp(&r), std::cmp::Ordering::Equal);
            // Conversions must never panic; exactness invariant: an
            // in-range integral to_i64 must reparse to the same value.
            if let Some(i) = n.to_i64() {
                assert_eq!(Number::from(i), n, "to_i64 exactness for {s:?}");
            }
            let _ = (n.to_u64(), n.to_i128(), n.to_u128(), n.to_f64(), n.to_f32());
        }
    }
});
