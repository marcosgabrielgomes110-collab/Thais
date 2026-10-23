//! Session event envelopes carried over the Agent API.

use serde::{Deserialize, Serialize};

use crate::error::ProtocolError;

/// The kind of event appended to the Event Log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    /// A session was created.
    SessionCreated,
    /// A turn started.
    TurnStarted,
    /// A model text delta.
    ModelDelta,
    /// A model reasoning delta.
    ReasoningDelta,
    /// A tool call was proposed.
    ToolCall,
    /// A tool call returned a result.
    ToolResult,
    /// An action is waiting for user approval.
    PermissionRequested,
    /// An approval or denial was recorded.
    PermissionResolved,
    /// A turn completed successfully.
    TurnCompleted,
    /// A turn was cancelled.
    TurnCancelled,
    /// An error was recorded.
    Error,
}

impl EventKind {
    /// Stable string representation stored in SQLite.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SessionCreated => "session_created",
            Self::TurnStarted => "turn_started",
            Self::ModelDelta => "model_delta",
            Self::ReasoningDelta => "reasoning_delta",
            Self::ToolCall => "tool_call",
            Self::ToolResult => "tool_result",
            Self::PermissionRequested => "permission_requested",
            Self::PermissionResolved => "permission_resolved",
            Self::TurnCompleted => "turn_completed",
            Self::TurnCancelled => "turn_cancelled",
            Self::Error => "error",
        }
    }

    /// Parse a stored string representation.
    pub fn parse(value: &str) -> Result<Self, ProtocolError> {
        match value {
            "session_created" => Ok(Self::SessionCreated),
            "turn_started" => Ok(Self::TurnStarted),
            "model_delta" => Ok(Self::ModelDelta),
            "reasoning_delta" => Ok(Self::ReasoningDelta),
            "tool_call" => Ok(Self::ToolCall),
            "tool_result" => Ok(Self::ToolResult),
            "permission_requested" => Ok(Self::PermissionRequested),
            "permission_resolved" => Ok(Self::PermissionResolved),
            "turn_completed" => Ok(Self::TurnCompleted),
            "turn_cancelled" => Ok(Self::TurnCancelled),
            "error" => Ok(Self::Error),
            other => Err(ProtocolError::InvalidParams(format!(
                "unknown event kind: {other}"
            ))),
        }
    }
}

/// One append-only event in a session's history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventEnvelope {
    /// Stable event identifier.
    pub event_id: String,
    /// Session this event belongs to.
    pub session_id: String,
    /// Monotonic per-session sequence number, starting at 1.
    pub seq: u64,
    /// RFC 3339 timestamp.
    pub timestamp: String,
    /// Event kind.
    pub kind: EventKind,
    /// Event-specific payload.
    pub payload: serde_json::Value,
}

impl EventEnvelope {
    /// Build a new event envelope.
    #[must_use]
    pub fn new(
        event_id: impl Into<String>,
        session_id: impl Into<String>,
        seq: u64,
        timestamp: impl Into<String>,
        kind: EventKind,
        payload: serde_json::Value,
    ) -> Self {
        Self {
            event_id: event_id.into(),
            session_id: session_id.into(),
            seq,
            timestamp: timestamp.into(),
            kind,
            payload,
        }
    }
}
