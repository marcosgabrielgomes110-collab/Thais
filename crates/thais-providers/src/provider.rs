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

impl Role {
    /// Stable wire representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Tool => "tool",
        }
    }
}

/// One normalized chat message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatMessage {
    /// Message role.
    pub role: Role,
    /// Text content. Tool-call-only assistant messages may omit it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// Assistant tool calls.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    /// Tool call this message answers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl ChatMessage {
    /// Build a system message.
    #[must_use]
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: Some(content.into()),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    /// Build a user message.
    #[must_use]
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: Some(content.into()),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    /// Build an assistant message.
    #[must_use]
    pub fn assistant(content: Option<String>, tool_calls: Vec<ToolCall>) -> Self {
        Self {
            role: Role::Assistant,
            content,
            tool_calls,
            tool_call_id: None,
        }
    }

    /// Build a tool result message.
    #[must_use]
    pub fn tool(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: Some(content.into()),
            tool_calls: Vec::new(),
            tool_call_id: Some(tool_call_id.into()),
        }
    }
}

/// A completed tool call proposed by the model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    /// Provider-assigned tool call identifier.
    pub id: String,
    /// Tool name.
    pub name: String,
    /// JSON arguments as emitted by the model.
    pub arguments: String,
}

/// A tool exposed to the model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolDefinition {
    /// Tool name.
    pub name: String,
    /// Short description.
    pub description: String,
    /// JSON Schema for the arguments.
    pub parameters: serde_json::Value,
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
    /// Tools available to the model.
    #[serde(default)]
    pub tools: Vec<ToolDefinition>,
    /// Provider-specific options.
    #[serde(default)]
    pub extra: serde_json::Value,
}

impl ChatRequest {
    /// Build a one-message user request.
    #[must_use]
    pub fn user(model: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            messages: vec![ChatMessage::user(content)],
            ..Self::default()
        }
    }
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
    /// HTTP status returned by the provider.
    #[error("provider HTTP {status}: {message}")]
    Http {
        /// HTTP status code.
        status: u16,
        /// Redacted response summary.
        message: String,
    },
    /// The provider did not respond within the configured deadline.
    #[error("provider timeout: {0}")]
    Timeout(String),
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
