//! Structural normalization: reorder coarse→fine, drop zeros, merge duplicates.

use super::{DurationSegment, DurationUnit};

/// Reorder segments coarse→fine, drop zero magnitudes, optionally merge
/// duplicate units (coercion only — authored source diagnoses duplicates
/// as NML3007 instead).
///
/// `Err(unit)` reports a merge whose magnitude overflows `u64` — the
/// caller maps it to the out-of-range rejection. Never saturates: a
/// silently clamped magnitude would be a wrong value, and wrong values
/// are the one thing the duration domain exists to prevent.
pub(crate) fn canonicalize(
    segments: &[DurationSegment],
    merge_duplicates: bool,
) -> Result<Vec<DurationSegment>, DurationUnit> {
    let mut out: Vec<DurationSegment> = Vec::new();
    for seg in segments {
        if seg.magnitude == 0 {
            continue;
        }
        if merge_duplicates {
            if let Some(existing) = out.iter_mut().find(|s| s.unit == seg.unit) {
                existing.magnitude = existing
                    .magnitude
                    .checked_add(seg.magnitude)
                    .ok_or(seg.unit)?;
                continue;
            }
        }
        out.push(*seg);
    }
    if out.is_empty() {
        // The canonical zero: every unit's zero is the same value, so it
        // has ONE spelling (`0s`) — the segment model cannot carry a unit
        // for a value with no nonzero component.
        out.push(DurationSegment {
            magnitude: 0,
            unit: DurationUnit::Seconds,
        });
    }
    out.sort_by_key(|s| unit_rank(s.unit));
    Ok(out)
}

pub(crate) fn unit_rank(unit: DurationUnit) -> u8 {
    DurationUnit::ALL
        .iter()
        .position(|u| *u == unit)
        .expect("ALL is total over the enum by construction") as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(magnitude: u64, unit: DurationUnit) -> DurationSegment {
        DurationSegment { magnitude, unit }
    }

    #[test]
    fn reorders_coarse_to_fine() {
        let segs = canonicalize(
            &[seg(30, DurationUnit::Minutes), seg(1, DurationUnit::Hours)],
            false,
        )
        .unwrap();
        assert_eq!(segs[0].unit, DurationUnit::Hours);
        assert_eq!(segs[1].unit, DurationUnit::Minutes);
    }

    #[test]
    fn all_zero_becomes_the_canonical_zero() {
        let segs = canonicalize(&[seg(0, DurationUnit::Milliseconds)], false).unwrap();
        assert_eq!(segs, vec![seg(0, DurationUnit::Seconds)]);
        let segs = canonicalize(&[], true).unwrap();
        assert_eq!(segs, vec![seg(0, DurationUnit::Seconds)]);
    }

    #[test]
    fn merge_folds_duplicates_exactly() {
        let segs = canonicalize(
            &[seg(1, DurationUnit::Hours), seg(2, DurationUnit::Hours)],
            true,
        )
        .unwrap();
        assert_eq!(segs, vec![seg(3, DurationUnit::Hours)]);
    }

    #[test]
    fn merge_overflow_is_rejected_never_saturated() {
        // Two magnitudes summing past u64 must ERROR: a saturated sum
        // would be an in-domain but WRONG value (u64::MAX ns is far
        // below the total-nanos ceiling, so the gate would accept it).
        let err = canonicalize(
            &[
                seg(u64::MAX, DurationUnit::Nanoseconds),
                seg(1, DurationUnit::Nanoseconds),
            ],
            true,
        )
        .unwrap_err();
        assert_eq!(err, DurationUnit::Nanoseconds);
    }
}
