//! Provider trait and normalized chat types.

use async_trait::async_trait;
use futures_util::stream::BoxStream;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// A model exposed by a provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Model {
    /// Model identifier used on the wire.
    pub id: String,
    /// Human-readable name.
    #[serde(default)]
    pub name: String,
    /// Context window in tokens.
    #[serde(default)]
    pub context_window: u64,
    /// Whether the model supports tool calling.
    #[serde(default)]
    pub supports_tools: bool,
}

/// Role of a chat message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// System instructions.
    System,
    /// User input.
    User,
    /// Assistant output.
    Assistant,
    /// Tool result.
    Tool,
}

/// One normalized chat message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatMessage {
    /// Message role.
    pub role: Role,
    /// Message content.
    pub content: String,
}

/// A normalized chat request.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChatRequest {
    /// Model identifier.
    #[serde(default)]
    pub model: String,
    /// Conversation history.
    #[serde(default)]
    pub messages: Vec<ChatMessage>,
    /// Optional sampling temperature.
    #[serde(default)]
    pub temperature: Option<f32>,
    /// Provider-specific options.
    #[serde(default)]
    pub extra: serde_json::Value,
}

/// Token usage reported by a provider.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    /// Input tokens.
    pub input_tokens: u64,
    /// Output tokens.
    pub output_tokens: u64,
}

/// Normalized streaming event from a provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ChatEvent {
    /// Text content delta.
    Content {
        /// Delta text.
        text: String,
    },
    /// Reasoning delta.
    Reasoning {
        /// Delta text.
        text: String,
    },
    /// Tool-call delta accumulated by index.
    ToolCallDelta {
        /// Tool-call index.
        index: usize,
        /// Tool-call identifier when known.
        #[serde(default)]
        id: Option<String>,
        /// Tool name when known.
        #[serde(default)]
        name: Option<String>,
        /// Partial JSON arguments.
        #[serde(default)]
        arguments: String,
    },
    /// Usage report.
    Usage {
        /// Input and output tokens.
        usage: Usage,
    },
    /// Stream completed.
    Done,
}

/// Provider failures.
#[derive(Debug, Error)]
pub enum ProviderError {
    /// Generic provider error.
    #[error("provider error: {0}")]
    Message(String),
    /// The requested model is not available.
    #[error("unsupported model: {0}")]
    UnsupportedModel(String),
}

/// Provider adapter interface.
#[async_trait]
pub trait Provider: Send + Sync {
    /// Stable provider identifier.
    fn id(&self) -> &str;

    /// Models available from this provider.
    async fn models(&self) -> Result<Vec<Model>, ProviderError>;

    /// Start a streaming chat request.
    async fn chat(
        &self,
        request: ChatRequest,
    ) -> Result<BoxStream<'static, Result<ChatEvent, ProviderError>>, ProviderError>;
}
