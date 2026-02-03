//! Deterministic entropy providers using Temporal's workflow context.
//!
//! These implementations use Temporal's deterministic random and time
//! primitives to ensure workflow replay correctness.

use std::ops::Range;
use std::time::{Duration, SystemTime};

/// Random source that uses Temporal's deterministic random.
///
/// During replay, Temporal provides the same random values that were
/// generated during the original execution.
#[derive(Debug)]
pub struct WorkflowRandomSource {
    seed: u64,
    counter: std::cell::Cell<u64>,
}

impl WorkflowRandomSource {
    /// Create a new workflow random source with the given seed.
    pub fn new(seed: u64) -> Self {
        Self {
            seed,
            counter: std::cell::Cell::new(0),
        }
    }

    /// Generate a deterministic random u64.
    fn next_u64(&self) -> u64 {
        let counter = self.counter.get();
        self.counter.set(counter.wrapping_add(1));

        // Simple deterministic PRNG based on seed and counter
        let mut state = self.seed.wrapping_add(counter);
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        state.wrapping_mul(0x2545F4914F6CDD1D)
    }

    /// Generate a UUID string.
    pub fn uuid(&self) -> String {
        let a = self.next_u64();
        let b = self.next_u64();

        // Format as UUID v4
        format!(
            "{:08x}-{:04x}-4{:03x}-{:04x}-{:012x}",
            (a >> 32) as u32,
            ((a >> 16) & 0xFFFF) as u16,
            ((a >> 4) & 0x0FFF) as u16,
            (((b >> 48) & 0x3FFF) | 0x8000) as u16,
            b & 0xFFFFFFFFFFFF,
        )
    }

    /// Generate a random f64 in [0, 1).
    pub fn f64(&self) -> f64 {
        (self.next_u64() as f64) / (u64::MAX as f64)
    }

    /// Generate a random u64.
    pub fn u64(&self) -> u64 {
        self.next_u64()
    }

    /// Generate a random f64 in the given range.
    pub fn f64_range(&self, range: Range<f64>) -> f64 {
        range.start + self.f64() * (range.end - range.start)
    }
}

/// Clock that uses Temporal's workflow time.
///
/// During replay, Temporal provides the same timestamps that were
/// observed during the original execution.
#[derive(Debug)]
pub struct WorkflowClock {
    workflow_time: SystemTime,
}

impl WorkflowClock {
    /// Create a new workflow clock with the given time.
    pub fn new(workflow_time: SystemTime) -> Self {
        Self { workflow_time }
    }

    /// Returns the workflow's current time (not wall clock).
    pub fn now(&self) -> Duration {
        self.workflow_time
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
    }

    /// Returns the workflow's current time as SystemTime.
    pub fn wall_time(&self) -> SystemTime {
        self.workflow_time
    }

    /// Returns milliseconds since Unix epoch.
    pub fn unix_millis(&self) -> u64 {
        self.workflow_time
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workflow_random_source_is_deterministic() {
        let source1 = WorkflowRandomSource::new(12345);
        let source2 = WorkflowRandomSource::new(12345);

        // Same seed should produce same values
        assert_eq!(source1.u64(), source2.u64());
        assert_eq!(source1.f64(), source2.f64());
        assert_eq!(source1.uuid(), source2.uuid());
    }

    #[test]
    fn workflow_random_source_different_seeds() {
        let source1 = WorkflowRandomSource::new(12345);
        let source2 = WorkflowRandomSource::new(54321);

        // Different seeds should produce different values
        assert_ne!(source1.u64(), source2.u64());
    }

    #[test]
    fn workflow_random_source_f64_range() {
        let source = WorkflowRandomSource::new(12345);

        for _ in 0..100 {
            let val = source.f64_range(0.5..1.5);
            assert!(val >= 0.5 && val < 1.5);
        }
    }

    #[test]
    fn workflow_clock_returns_configured_time() {
        let time = SystemTime::UNIX_EPOCH + Duration::from_secs(1000000);
        let clock = WorkflowClock::new(time);

        assert_eq!(clock.wall_time(), time);
        assert_eq!(clock.unix_millis(), 1000000000);
    }
}
