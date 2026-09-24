use super::*;

#[test]
#[ignore = "timing gate — run with --release -- --ignored"]
fn perf_deep_20x16_composes_within_bounds() {
    perf_compose("deep-20x16.nml", std::time::Duration::from_secs(10));
}

#[test]
#[ignore = "timing gate — run with --release -- --ignored"]
fn perf_deep_40x8_composes_within_bounds() {
    perf_compose("deep-40x8.nml", std::time::Duration::from_secs(10));
}

#[test]
#[ignore = "timing gate — run with --release -- --ignored"]
fn perf_wide_50x8x7_composes_within_bounds() {
    perf_compose("wide-50x8x7.nml", std::time::Duration::from_secs(10));
}

#[test]
#[ignore = "timing gate — run with --release -- --ignored"]
fn perf_nest_2000_composes_within_bounds() {
    perf_compose("nest-2000.nml", std::time::Duration::from_secs(20));
}

#[test]
#[ignore = "timing gate — run with --release -- --ignored"]
fn perf_nest_8000_composes_within_bounds() {
    perf_compose("nest-8000.nml", std::time::Duration::from_secs(60));
}
