//! Session models and method parameters/results.

use serde::{Deserialize, Serialize};

use crate::error::ProtocolError;
use crate::event::EventEnvelope;

/// Whether a session may mutate the workspace.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionMode {
    /// Read-only planning.
    Plan,
    /// Mutating build mode.
    #[default]
    Build,
}

impl SessionMode {
    /// Stable string representation stored in SQLite.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Plan => "plan",
            Self::Build => "build",
        }
    }

    /// Parse a stored string representation.
    pub fn parse(value: &str) -> Result<Self, ProtocolError> {
        match value {
            "plan" => Ok(Self::Plan),
            "build" => Ok(Self::Build),
            other => Err(ProtocolError::InvalidParams(format!(
                "unknown session mode: {other}"
            ))),
        }
    }
}

/// Lifecycle state of a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    /// Session is active.
    Active,
    /// Session is archived and read-only.
    Archived,
    /// Session finished successfully.
    Completed,
    /// Session failed.
    Failed,
    /// Session was cancelled.
    Cancelled,
}

impl SessionStatus {
    /// Stable string representation stored in SQLite.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Archived => "archived",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    /// Parse a stored string representation.
    pub fn parse(value: &str) -> Result<Self, ProtocolError> {
        match value {
            "active" => Ok(Self::Active),
            "archived" => Ok(Self::Archived),
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            "cancelled" => Ok(Self::Cancelled),
            other => Err(ProtocolError::InvalidParams(format!(
                "unknown session status: {other}"
            ))),
        }
    }
}

/// Public session metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInfo {
    /// Stable session identifier.
    pub session_id: String,
    /// Workspace path supplied at creation.
    pub workspace: String,
    /// Active mode.
    pub mode: SessionMode,
    /// Selected model identifier.
    pub model: String,
    /// Lifecycle state.
    pub status: SessionStatus,
    /// RFC 3339 creation timestamp.
    pub created_at: String,
    /// RFC 3339 last-update timestamp.
    pub updated_at: String,
}

/// Parameters for `initialize`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InitializeParams {
    /// Client name.
    #[serde(default)]
    pub client_name: String,
    /// Client version.
    #[serde(default)]
    pub client_version: String,
    /// Protocol version the client expects.
    #[serde(default)]
    pub protocol_version: String,
    /// Optional capability map.
    #[serde(default)]
    pub capabilities: serde_json::Value,
}

/// Result of `initialize`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitializeResult {
    /// Server name.
    pub server_name: String,
    /// Server version.
    pub server_version: String,
    /// Protocol version the server speaks.
    pub protocol_version: String,
    /// Optional capability map.
    pub capabilities: serde_json::Value,
}

/// Parameters for `session.create`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionCreateParams {
    /// Workspace root.
    pub workspace: String,
    /// Initial mode.
    #[serde(default)]
    pub mode: SessionMode,
    /// Optional model override.
    #[serde(default)]
    pub model: Option<String>,
}

/// Result of `session.create`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionCreateResult {
    /// New session.
    pub session: SessionInfo,
}

/// Parameters for `session.send`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSendParams {
    /// Target session.
    pub session_id: String,
    /// User input.
    pub input: String,
    /// Optional client-supplied turn identifier.
    #[serde(default)]
    pub turn_id: Option<String>,
}

/// Result of `session.send`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSendResult {
    /// Turn identifier assigned by the server.
    pub turn_id: String,
}

/// Parameters for `session.cancel`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionCancelParams {
    /// Target session.
    pub session_id: String,
    /// Optional turn to cancel; the active turn when omitted.
    #[serde(default)]
    pub turn_id: Option<String>,
}

/// Result of `session.cancel`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionCancelResult {
    /// Whether an in-flight turn was cancelled.
    pub cancelled: bool,
}

/// Decision supplied to `session.approve`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    /// Allow the action.
    Allow,
    /// Deny the action.
    Deny,
}

/// Parameters for `session.approve`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionApproveParams {
    /// Target session.
    pub session_id: String,
    /// Approval identifier from `permission.requested`.
    pub approval_id: String,
    /// Allow or deny.
    pub decision: ApprovalDecision,
}

/// Result of `session.approve`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionApproveResult {
    /// Whether a pending approval was resolved.
    pub resolved: bool,
}

/// Parameters for `session.subscribe`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSubscribeParams {
    /// Target session.
    pub session_id: String,
    /// Replay events after this sequence number. Omit for live-only.
    #[serde(default)]
    pub from_seq: Option<u64>,
}

/// Result of `session.subscribe`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSubscribeResult {
    /// Subscription identifier.
    pub subscription_id: String,
    /// First sequence number that will be observed.
    pub from_seq: u64,
}

/// Parameters for the `session.event` notification.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEventParams {
    /// Event payload.
    pub event: EventEnvelope,
}
