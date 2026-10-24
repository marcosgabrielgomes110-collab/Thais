#![forbid(unsafe_code)]

//! Headless core: session actors, the turn loop, and the Agent API server.

pub mod clock;
pub mod context;
pub mod env_badge;
pub mod ids;
pub mod runtime;
pub mod server;

pub use clock::{Clock, FixedClock, SystemClock};
pub use context::{ContextError, ContextManager, PromptBundle, estimate_tokens};
pub use env_badge::EnvironmentBadge;
pub use ids::{IdGenerator, SequenceIdGenerator, UuidGenerator};
pub use runtime::{RequestOutcome, Runtime, RuntimeError};
pub use server::{ServerError, serve, serve_stdio};

/// Version of the core crate.
pub const CORE_VERSION: &str = env!("CARGO_PKG_VERSION");
