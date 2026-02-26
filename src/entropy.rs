//! Deterministic entropy providers backed by the Temporal workflow context.
//!
//! Temporal workflows must be deterministic — no direct calls to
//! `Uuid::new_v4()`.  This module provides a deterministic random source
//! seeded from the workflow's random seed.

use std::fmt::Debug;
use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};
use codex_core::entropy::RandomSource;

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

