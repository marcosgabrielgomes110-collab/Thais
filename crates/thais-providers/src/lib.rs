#![forbid(unsafe_code)]

//! Provider adapter layer. M0 ships the trait and a fake provider; native
//! adapters arrive in M3.

pub mod fake;
pub mod openai_compatible;
pub mod provider;

pub use fake::{FakeProvider, FakeProviderError, FakeScript, FakeStep};
pub use openai_compatible::{COHERE_BASE_URL, COHERE_DEFAULT_MODEL, OpenAiCompatibleProvider};
pub use provider::{
    ChatEvent, ChatMessage, ChatRequest, Model, Provider, ProviderError, Role, ToolCall,
    ToolDefinition, Usage,
};

/// Version of the internal provider API.
pub const PROVIDER_API_VERSION: &str = "0.1";
