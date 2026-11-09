use std::time::Duration;

use super::since_cutoff;

#[test]
fn since_is_an_absolute_time_before_now() {
    assert_eq!(
        since_cutoff(10_000_000, Duration::from_secs(3600)),
        6_400_000
    );
}

#[test]
fn since_longer_than_the_clock_reaches_back_to_zero() {
    assert_eq!(since_cutoff(1_000, Duration::from_secs(3600)), 0);
    assert_eq!(since_cutoff(1_000, Duration::MAX), 0);
}
