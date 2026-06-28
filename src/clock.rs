// SPDX-License-Identifier: Apache-2.0
//
// Time source. The daemon stamps every captured byte and every log line with both a
// monotonic clock (for "how long after a reset did X happen", immune to wall-clock jumps)
// and wall-clock epoch ms (for correlating with other logs). Behind a trait so tests can
// inject a deterministic clock.

use std::time::{Instant, SystemTime, UNIX_EPOCH};

pub trait Clock: Send + Sync {
    /// Returns (monotonic nanoseconds since some fixed base, wall-clock epoch milliseconds).
    fn now(&self) -> (u64, u64);
}

/// Real clock: monotonic ns measured from process start, wall ms from the system clock.
pub struct SystemClock {
    base: Instant,
}

impl SystemClock {
    pub fn new() -> Self {
        SystemClock {
            base: Instant::now(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn now(&self) -> (u64, u64) {
        let mono = self.base.elapsed().as_nanos() as u64;
        let wall = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        (mono, wall)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_clock_is_monotonic_nondecreasing() {
        let c = SystemClock::new();
        let (m1, w1) = c.now();
        let (m2, w2) = c.now();
        assert!(m2 >= m1, "monotonic went backwards: {m1} -> {m2}");
        assert!(w1 > 0 && w2 > 0, "wall clock should be a real epoch");
    }
}
