//! Smoothed event rates (Livestatus `*_rate` columns).

/// Tracks a monotonically increasing counter and derives a smoothed per-second rate,
/// similar to Livestatus' exponential averaging.
#[derive(Debug, Clone, Default)]
pub struct RateTracker {
    last_time: f64,
    last_value: u64,
    rate: f64,
}

impl RateTracker {
    pub const fn new() -> Self {
        RateTracker { last_time: 0.0, last_value: 0, rate: 0.0 }
    }

    /// Minimum interval between two samples.
    const MIN_INTERVAL: f64 = 5.0;

    /// Feed the current counter value; the rate is updated at most every 5 s.
    pub fn sample(&mut self, now: f64, value: u64) {
        if self.last_time == 0.0 {
            self.last_time = now;
            self.last_value = value;
            return;
        }
        let dt = now - self.last_time;
        if dt < Self::MIN_INTERVAL {
            return;
        }
        let current = value.saturating_sub(self.last_value) as f64 / dt;
        self.rate = if self.rate == 0.0 { current } else { 0.5 * self.rate + 0.5 * current };
        self.last_time = now;
        self.last_value = value;
    }

    pub fn rate(&self) -> f64 {
        self.rate
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn computes_and_smooths() {
        let mut r = RateTracker::default();
        r.sample(100.0, 0);
        r.sample(102.0, 10); // too early, ignored
        assert_eq!(r.rate(), 0.0);
        r.sample(110.0, 100);
        assert_eq!(r.rate(), 10.0);
        r.sample(120.0, 100);
        assert_eq!(r.rate(), 5.0);
    }
}
