//! Session event envelopes carried over the Agent API.

use serde::{Deserialize, Serialize};

use crate::error::ProtocolError;

/// The kind of event appended to the Event Log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    /// A session was created.
    SessionCreated,
    /// The model policy changed.
    ModelChanged,
    /// A budget threshold was crossed.
    BudgetWarning,
    /// A pre-action snapshot was created.
    SnapshotCreated,
    /// A rollback restored a snapshot.
    RollbackCompleted,
    /// Verification started.
    VerificationStarted,
    /// Verification passed.
    VerificationPassed,
    /// Verification failed.
    VerificationFailed,
    /// Verification could not run.
    VerificationUnavailable,
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
    /// The gate allowed an action without a prompt.
    PermissionGranted,
    /// The gate denied an action.
    PermissionDenied,
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
            Self::ModelChanged => "model_changed",
            Self::BudgetWarning => "budget_warning",
            Self::SnapshotCreated => "snapshot_created",
            Self::RollbackCompleted => "rollback_completed",
            Self::VerificationStarted => "verification_started",
            Self::VerificationPassed => "verification_passed",
            Self::VerificationFailed => "verification_failed",
            Self::VerificationUnavailable => "verification_unavailable",
            Self::TurnStarted => "turn_started",
            Self::ModelDelta => "model_delta",
            Self::ReasoningDelta => "reasoning_delta",
            Self::ToolCall => "tool_call",
            Self::ToolResult => "tool_result",
            Self::PermissionRequested => "permission_requested",
            Self::PermissionGranted => "permission_granted",
            Self::PermissionDenied => "permission_denied",
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
            "model_changed" => Ok(Self::ModelChanged),
            "budget_warning" => Ok(Self::BudgetWarning),
            "snapshot_created" => Ok(Self::SnapshotCreated),
            "rollback_completed" => Ok(Self::RollbackCompleted),
            "verification_started" => Ok(Self::VerificationStarted),
            "verification_passed" => Ok(Self::VerificationPassed),
            "verification_failed" => Ok(Self::VerificationFailed),
            "verification_unavailable" => Ok(Self::VerificationUnavailable),
            "turn_started" => Ok(Self::TurnStarted),
            "model_delta" => Ok(Self::ModelDelta),
            "reasoning_delta" => Ok(Self::ReasoningDelta),
            "tool_call" => Ok(Self::ToolCall),
            "tool_result" => Ok(Self::ToolResult),
            "permission_requested" => Ok(Self::PermissionRequested),
            "permission_granted" => Ok(Self::PermissionGranted),
            "permission_denied" => Ok(Self::PermissionDenied),
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
