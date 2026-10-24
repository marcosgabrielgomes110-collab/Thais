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
    /// Session was soft-deleted.
    Deleted,
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
            Self::Deleted => "deleted",
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
            "deleted" => Ok(Self::Deleted),
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionInfo {
    /// Stable session identifier.
    pub session_id: String,
    /// Parent session for subagent child sessions.
    #[serde(default)]
    pub parent_id: Option<String>,
    /// Human-readable session name.
    #[serde(default)]
    pub name: String,
    /// Workspace path supplied at creation.
    pub workspace: String,
    /// Active mode.
    pub mode: SessionMode,
    /// Selected default model identifier.
    pub model: String,
    /// Role-based model policy.
    #[serde(default)]
    pub model_policy: serde_json::Value,
    /// Cumulative input tokens for the session.
    #[serde(default)]
    pub input_tokens: u64,
    /// Cumulative output tokens for the session.
    #[serde(default)]
    pub output_tokens: u64,
    /// Cumulative cost in USD for the session.
    #[serde(default)]
    pub cost_usd: f64,
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
    /// Optional session name.
    #[serde(default)]
    pub name: Option<String>,
    /// Optional parent session for subagent-created sessions.
    #[serde(default)]
    pub parent_id: Option<String>,
    /// Initial mode.
    #[serde(default)]
    pub mode: SessionMode,
    /// Optional model override.
    #[serde(default)]
    pub model: Option<String>,
    /// Take over an existing mutating lease after confirmation.
    #[serde(default)]
    pub steal_lease: bool,
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
    /// Optional per-turn model override.
    #[serde(default)]
    pub model: Option<String>,
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

/// Parameters for `session.list`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionListParams {
    /// Include archived sessions.
    #[serde(default)]
    pub include_archived: bool,
}

/// Result of `session.list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionListResult {
    /// Matching sessions.
    pub sessions: Vec<SessionInfo>,
}

/// Parameters for `session.get`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionGetParams {
    /// Target session.
    pub session_id: String,
}

/// Result of `session.get`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionGetResult {
    /// Session metadata.
    pub session: SessionInfo,
}

/// Parameters for `session.rename`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRenameParams {
    /// Target session.
    pub session_id: String,
    /// New name.
    pub name: String,
}

/// Result of `session.rename`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRenameResult {
    /// Updated session.
    pub session: SessionInfo,
}

/// Parameters for `session.archive`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionArchiveParams {
    /// Target session.
    pub session_id: String,
}

/// Result of `session.archive`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionArchiveResult {
    /// Updated session.
    pub session: SessionInfo,
}

/// Parameters for `session.delete`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionDeleteParams {
    /// Target session.
    pub session_id: String,
}

/// Result of `session.delete`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionDeleteResult {
    /// Whether the session was soft-deleted.
    pub deleted: bool,
}

/// Parameters for `session.resume`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionResumeParams {
    /// Target session.
    pub session_id: String,
}

/// A durable pending approval.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalInfo {
    /// Approval identifier.
    pub approval_id: String,
    /// Session identifier.
    pub session_id: String,
    /// Action JSON.
    pub action: serde_json::Value,
    /// RFC 3339 creation timestamp.
    pub created_at: String,
}

/// Checkpoint summary returned on resume.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CheckpointSummary {
    /// Current loop state.
    #[serde(default)]
    pub state: String,
    /// Approved plan, when present.
    #[serde(default)]
    pub plan: Option<String>,
    /// Derived task list.
    #[serde(default)]
    pub tasks: Vec<String>,
    /// Compacted summary of older turns.
    #[serde(default)]
    pub summary: Option<String>,
}

/// Result of `session.resume`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionResumeResult {
    /// Restored session.
    pub session: SessionInfo,
    /// Checkpoint summary, when present.
    #[serde(default)]
    pub checkpoint: Option<CheckpointSummary>,
    /// Pending approvals still open.
    #[serde(default)]
    pub pending_approvals: Vec<ApprovalInfo>,
}

/// Parameters for `session.set_model`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSetModelParams {
    /// Target session.
    pub session_id: String,
    /// Role name: `default`, `plan`, `build`, `explore`, `general`, or `compact`.
    pub role: String,
    /// Model identifier.
    pub model: String,
}

/// Result of `session.set_model`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSetModelResult {
    /// Updated session.
    pub session: SessionInfo,
}

/// A model catalog entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    /// Model identifier.
    pub id: String,
    /// Provider identifier.
    pub provider: String,
    /// Maximum context window.
    pub context_window: u64,
    /// Whether the model supports tool calling.
    pub supports_tools: bool,
    /// Input cost per million tokens.
    pub input_cost_per_million: f64,
    /// Output cost per million tokens.
    pub output_cost_per_million: f64,
}

/// Result of `models.list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelsListResult {
    /// Available models.
    pub models: Vec<ModelInfo>,
}

/// Parameters for `history.search`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistorySearchParams {
    /// FTS5 query.
    pub query: String,
    /// Optional session filter.
    #[serde(default)]
    pub session_id: Option<String>,
    /// Optional workspace filter.
    #[serde(default)]
    pub workspace: Option<String>,
    /// Maximum number of matches.
    #[serde(default)]
    pub limit: Option<u32>,
}

/// One history search hit with its session label.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryMatch {
    /// Matching event.
    pub event: crate::event::EventEnvelope,
    /// Session name.
    pub session_name: String,
    /// Canonical workspace.
    pub workspace: String,
}

/// Result of `history.search`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistorySearchResult {
    /// Matching events.
    pub matches: Vec<HistoryMatch>,
}

/// Parameters for `session.release_lease`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionReleaseLeaseParams {
    /// Target session.
    pub session_id: String,
}

/// Result of `session.release_lease`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionReleaseLeaseResult {
    /// Whether a lease was released.
    pub released: bool,
}
