//! `SystemClock` — the [`Clock`] backed by the OS wall clock.

use crate::core::deps::Clock;
use std::time::{SystemTime, UNIX_EPOCH};

/// The production [`Clock`]: reads the OS wall clock in unix seconds.
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_unix(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0) // pre-1970 clock is not a real deployment
    }
}
