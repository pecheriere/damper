//! A running maximum that fades exponentially.

use std::time::{Duration, Instant};

/// A maximum that halves every `half_life` unless a larger value is observed.
#[derive(Debug, Clone)]
pub struct DecayingPeak {
    value: u64,
    half_life: Duration,
    updated: Instant,
}

impl DecayingPeak {
    /// Starts at `initial` as of `now`.
    #[must_use]
    pub fn new(initial: u64, half_life: Duration, now: Instant) -> Self {
        Self {
            value: initial,
            half_life,
            updated: now,
        }
    }

    /// Records `value`: the peak becomes the larger of it and the decayed peak.
    pub fn observe(&mut self, value: u64, now: Instant) {
        self.value = self.get(now).max(value);
        self.updated = now;
    }

    /// The peak as of `now`, faded by the time since the last observation.
    ///
    /// A `now` earlier than the last observation counts as no time passed.
    #[must_use]
    pub fn get(&self, now: Instant) -> u64 {
        let elapsed = now.saturating_duration_since(self.updated);
        if elapsed.is_zero() || self.value == 0 {
            return self.value;
        }
        if self.half_life.is_zero() {
            return 0;
        }
        let halvings = elapsed.as_secs_f64() / self.half_life.as_secs_f64();
        let factor = 0.5_f64.powf(halvings);
        faded(self.value, factor)
    }
}

/// `value * factor` for a `factor` in `[0, 1]`, never above `value`.
#[expect(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "factor is within [0, 1] and byte counts only need float precision; \
              the float-to-int cast saturates and the result is capped at value"
)]
fn faded(value: u64, factor: f64) -> u64 {
    let scaled = value as f64 * factor.clamp(0.0, 1.0);
    (scaled as u64).min(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HALF_LIFE: Duration = Duration::from_secs(180);

    #[test]
    fn peak_decays_by_half_per_half_life() {
        let t0 = Instant::now();
        let peak = DecayingPeak::new(1000, HALF_LIFE, t0);
        assert_eq!(peak.get(t0), 1000);
        assert_eq!(peak.get(t0 + HALF_LIFE), 500);
        assert_eq!(peak.get(t0 + HALF_LIFE * 2), 250);
    }

    #[test]
    fn peak_takes_new_max_immediately() {
        let t0 = Instant::now();
        let mut peak = DecayingPeak::new(1000, HALF_LIFE, t0);
        peak.observe(5000, t0 + Duration::from_secs(1));
        assert_eq!(peak.get(t0 + Duration::from_secs(1)), 5000);
        peak.observe(10, t0 + Duration::from_secs(1));
        assert_eq!(peak.get(t0 + Duration::from_secs(1)), 5000);
    }

    #[test]
    fn peak_survives_odd_inputs() {
        let t0 = Instant::now();
        let peak = DecayingPeak::new(u64::MAX, HALF_LIFE, t0 + Duration::from_secs(5));
        assert_eq!(peak.get(t0), u64::MAX);
        let instant = DecayingPeak::new(7, Duration::ZERO, t0);
        assert_eq!(instant.get(t0), 7);
        assert_eq!(instant.get(t0 + Duration::from_secs(1)), 0);
    }
}
