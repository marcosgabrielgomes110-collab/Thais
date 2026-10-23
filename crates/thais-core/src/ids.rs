//! Injectable identifier generation for deterministic event streams.

use std::sync::atomic::{AtomicU64, Ordering};

use uuid::Uuid;

/// Generates stable identifiers for sessions, turns, events, approvals, and
/// subscriptions.
pub trait IdGenerator: Send + Sync {
    /// Generate an identifier for the given kind.
    fn new_id(&self, kind: &str) -> String;
}

/// UUID-based identifiers for production.
#[derive(Debug, Default, Clone, Copy)]
pub struct UuidGenerator;

impl IdGenerator for UuidGenerator {
    fn new_id(&self, kind: &str) -> String {
        format!("{kind}_{}", Uuid::new_v4().simple())
    }
}

/// Sequence-based identifiers for golden tests.
#[derive(Debug)]
pub struct SequenceIdGenerator {
    prefix: String,
    counter: AtomicU64,
}

impl SequenceIdGenerator {
    /// Build a generator with a stable prefix.
    #[must_use]
    pub fn new(prefix: impl Into<String>) -> Self {
        Self {
            prefix: prefix.into(),
            counter: AtomicU64::new(0),
        }
    }
}

impl IdGenerator for SequenceIdGenerator {
    fn new_id(&self, kind: &str) -> String {
        let value = self.counter.fetch_add(1, Ordering::Relaxed) + 1;
        format!("{}-{}-{}", self.prefix, kind, value)
    }
}
