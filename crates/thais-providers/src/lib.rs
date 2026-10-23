#![forbid(unsafe_code)]

//! Provider adapter layer. M0 ships the trait and a fake provider; native
//! adapters arrive in M3.

pub mod fake;
pub mod provider;

pub use fake::{FakeProvider, FakeProviderError, FakeScript, FakeStep};
pub use provider::{
    ChatEvent, ChatMessage, ChatRequest, Model, Provider, ProviderError, Role, Usage,
};

/// Version of the internal provider API.
pub const PROVIDER_API_VERSION: &str = "0.1";
