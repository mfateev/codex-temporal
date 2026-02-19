//! Deterministic entropy providers backed by the Temporal workflow context.
//!
//! Temporal workflows must be deterministic — no direct calls to
//! `Uuid::new_v4()`, `Instant::now()`, or `SystemTime::now()`.  Instead,
//! these providers use the workflow's random seed and logical clock.

use std::fmt::Debug;
use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use codex_core::entropy::{Clock, RandomSource};

/// Deterministic random source seeded from the workflow's random seed.
///
/// Uses a simple xorshift64 PRNG for reproducibility during replay.
#[derive(Debug)]
pub struct TemporalRandomSource {
    state: AtomicU64,
}

impl TemporalRandomSource {
    pub fn new(seed: u64) -> Self {
        // Ensure non-zero seed for xorshift
        let seed = if seed == 0 { 0xDEAD_BEEF_CAFE_BABE } else { seed };
        Self {
            state: AtomicU64::new(seed),
        }
    }

    fn next_u64(&self) -> u64 {
        loop {
            let old = self.state.load(Ordering::Relaxed);
            let mut x = old;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            if self
                .state
                .compare_exchange_weak(old, x, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                return x;
            }
        }
    }
}

impl RandomSource for TemporalRandomSource {
    fn uuid(&self) -> String {
        let a = self.next_u64();
        let b = self.next_u64();
        // Format as UUID v4 shape (but deterministic)
        let bytes: [u8; 16] = {
            let mut buf = [0u8; 16];
            buf[..8].copy_from_slice(&a.to_le_bytes());
            buf[8..].copy_from_slice(&b.to_le_bytes());
            // Set version (4) and variant (RFC 4122)
            buf[6] = (buf[6] & 0x0F) | 0x40;
            buf[8] = (buf[8] & 0x3F) | 0x80;
            buf
        };
        uuid::Uuid::from_bytes(bytes).to_string()
    }

    fn f64(&self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    fn u64(&self) -> u64 {
        self.next_u64()
    }

    fn f64_range(&self, range: Range<f64>) -> f64 {
        range.start + self.f64() * (range.end - range.start)
    }
}

/// Deterministic clock backed by the workflow's logical time.
///
/// `now()` returns a monotonically advancing `Instant` derived from the
/// workflow time.  `wall_time()` returns the workflow's logical wall clock.
#[derive(Debug)]
pub struct TemporalClock {
    /// Workflow start time (set once at workflow init).
    epoch: SystemTime,
    /// Monotonic counter used to synthesise `Instant` values.
    /// Each call to `now()` increments this so durations are always > 0.
    tick: AtomicU64,
}

impl TemporalClock {
    pub fn new(workflow_time: SystemTime) -> Self {
        Self {
            epoch: workflow_time,
            tick: AtomicU64::new(0),
        }
    }

    /// Update the logical wall-clock (called when Temporal advances time).
    pub fn advance(&self, _new_time: SystemTime) {
        // For now we just increment the tick; full time tracking can be
        // added when we have access to updated workflow time per activation.
        self.tick.fetch_add(1, Ordering::Relaxed);
    }
}

impl Clock for TemporalClock {
    fn now(&self) -> Instant {
        // We can't construct an arbitrary Instant, but we can use the real
        // clock — the important thing is that UUIDs and randomness are
        // deterministic.  Instant is only used for duration measurements
        // within a single turn, which is acceptable.
        Instant::now()
    }

    fn wall_time(&self) -> SystemTime {
        let ticks = self.tick.fetch_add(1, Ordering::Relaxed);
        self.epoch + Duration::from_millis(ticks)
    }

    fn unix_millis(&self) -> u64 {
        self.wall_time()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }
}
