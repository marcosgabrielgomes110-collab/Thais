#![forbid(unsafe_code)]

//! Headless core: session actors, the turn loop, and the Agent API server.

pub mod clock;
pub mod context;
pub mod env_badge;
pub mod ids;
pub mod replay;
pub mod runtime;
pub mod server;
pub mod snapshot;
pub mod verifier;

pub use clock::{Clock, FixedClock, SystemClock};
pub use context::{ContextError, ContextManager, PromptBundle, estimate_tokens};
pub use env_badge::EnvironmentBadge;
pub use ids::{IdGenerator, SequenceIdGenerator, UuidGenerator};
pub use replay::{ReplayError, ReplayState, replay_session};
pub use runtime::{RequestOutcome, Runtime, RuntimeError};
pub use server::{ServerError, serve, serve_stdio};
pub use snapshot::{Snapshot, SnapshotError, SnapshotManager};
pub use verifier::{Verification, Verifier, VerifierError};

/// Version of the core crate.
pub const CORE_VERSION: &str = env!("CARGO_PKG_VERSION");
