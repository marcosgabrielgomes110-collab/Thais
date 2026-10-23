//! Injectable clock for deterministic event streams.

use chrono::Utc;

/// Source of RFC 3339 timestamps.
pub trait Clock: Send + Sync {
    /// Current timestamp as RFC 3339.
    fn now_rfc3339(&self) -> String;
}

/// Wall clock.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_rfc3339(&self) -> String {
        Utc::now().to_rfc3339()
    }
}

/// Fixed clock for golden tests.
#[derive(Debug, Clone)]
pub struct FixedClock {
    value: String,
}

impl FixedClock {
    /// Build a clock that always returns the supplied timestamp.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self {
            value: value.into(),
        }
    }
}

impl Clock for FixedClock {
    fn now_rfc3339(&self) -> String {
        self.value.clone()
    }
}
