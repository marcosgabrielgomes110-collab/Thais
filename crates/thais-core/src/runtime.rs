//! Session runtime: create sessions, run turns, stream events, and resolve
//! approvals and cancellations.

use std::collections::{BTreeMap, HashMap};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use serde::Serialize;
use serde_json::{Value, json};
use thais_protocol::PROTOCOL_VERSION;
use thais_protocol::error::{ErrorObject, INTERNAL_ERROR, INVALID_PARAMS, ProtocolError};
use thais_protocol::event::{EventEnvelope, EventKind};
use thais_protocol::jsonrpc::{Id, Request, Response};
use thais_protocol::method;
use thais_protocol::session::{
    ApprovalDecision, HistoryMatch, HistorySearchParams, HistorySearchResult, InitializeParams,
    InitializeResult, ModelInfo, ModelsListResult, SessionApproveParams, SessionApproveResult,
    SessionArchiveParams, SessionArchiveResult, SessionCancelParams, SessionCancelResult,
    SessionCreateParams, SessionCreateResult, SessionDeleteParams, SessionDeleteResult,
    SessionGetParams, SessionGetResult, SessionInfo, SessionListParams, SessionListResult,
    SessionMode, SessionReleaseLeaseParams, SessionReleaseLeaseResult, SessionRenameParams,
    SessionRenameResult, SessionResumeParams, SessionResumeResult, SessionSendParams,
    SessionSendResult, SessionSetModelParams, SessionSetModelResult, SessionStatus,
    SessionSubscribeParams, SessionSubscribeResult,
};
use thais_providers::{
    ChatEvent, ChatMessage, ChatRequest, Provider, ToolCall, ToolDefinition, Usage,
};
use thais_storage::{BasicRedactor, NewEvent, Redactor, StorageError, StorageHandle};
use thais_tools::{
    LspConfig, McpConfig, PermissionDecision, PermissionGate, PolicyMode, ToolError, ToolRegistry,
    ToolResult,
};
use thiserror::Error;
use tokio::sync::{Mutex, broadcast, oneshot};
use tokio_util::sync::CancellationToken;

use crate::checkpoint::{CheckpointState, read_checkpoint, write_checkpoint};
use crate::clock::Clock;
use crate::context::{ContextError, ContextManager, estimate_tokens};
use crate::ids::IdGenerator;
use crate::model_policy::ModelPolicy;
use crate::provider_registry::{CatalogModel, ProviderRegistry};
use crate::replay::{ReplayError, ReplayState};
use crate::snapshot::{SnapshotError, SnapshotManager};
use crate::verifier::{Verification, Verifier};

const MAX_TOOL_ITERATIONS: usize = 8;
const MAX_TURN_TOKENS: u64 = 50_000;
const MAX_TURN_TIME: Duration = Duration::from_secs(180);
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(60);
const LEASE_TTL_SECS: u64 = 300;
const DEFAULT_MAX_TURN_COST_USD: f64 = 2.0;
const DEFAULT_REBUILD_THRESHOLD_TOKENS: usize = 12_000;
const DEFAULT_HISTORY_TAIL_TURNS: usize = 4;
const SUBAGENT_MAX_ITERATIONS: usize = 4;
const SUBAGENT_MAX_TOKENS: u64 = 20_000;
const SUBAGENT_MAX_TIME: Duration = Duration::from_secs(120);

fn model_info(model: CatalogModel) -> ModelInfo {
    ModelInfo {
        id: model.id,
        provider: model.provider,
        context_window: model.context_window,
        supports_tools: model.supports_tools,
        input_cost_per_million: model.input_cost_per_million,
        output_cost_per_million: model.output_cost_per_million,
    }
}

/// Runtime failures.
#[derive(Debug, Error)]
pub enum RuntimeError {
    /// Storage failure.
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),
    /// The session does not exist.
    #[error("session not found: {0}")]
    SessionNotFound(String),
    /// A turn is already running in this session.
    #[error("turn already in progress: {0}")]
    TurnInProgress(String),
    /// Provider failure.
    #[error("provider error: {0}")]
    Provider(#[from] thais_providers::ProviderError),
    /// The approval is no longer pending.
    #[error("approval not found: {0}")]
    ApprovalNotFound(String),
    /// The request is not valid for this runtime.
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    /// The workspace path is not valid UTF-8.
    #[error("workspace path is not valid UTF-8")]
    InvalidWorkspace,
    /// Prompt assembly failure.
    #[error("context error: {0}")]
    Context(#[from] ContextError),
    /// Tool execution failure.
    #[error("tool error: {0}")]
    Tool(#[from] ToolError),
    /// Replay failure.
    #[error("replay error: {0}")]
    Replay(#[from] ReplayError),
    /// Snapshot failure.
    #[error("snapshot error: {0}")]
    Snapshot(#[from] SnapshotError),
    /// The workspace is leased by another mutating session.
    #[error("workspace is leased by session {0}")]
    LeaseHeld(String),
    /// Checkpoint failure.
    #[error("checkpoint error: {0}")]
    Checkpoint(#[from] crate::checkpoint::CheckpointError),
}

impl RuntimeError {
    fn to_error_object(&self) -> ErrorObject {
        let code = match self {
            Self::SessionNotFound(_)
            | Self::TurnInProgress(_)
            | Self::ApprovalNotFound(_)
            | Self::InvalidRequest(_)
            | Self::InvalidWorkspace
            | Self::LeaseHeld(_) => INVALID_PARAMS,
            Self::Storage(_)
            | Self::Provider(_)
            | Self::Context(_)
            | Self::Tool(_)
            | Self::Replay(_)
            | Self::Snapshot(_)
            | Self::Checkpoint(_) => INTERNAL_ERROR,
        };
        ErrorObject::new(code, self.to_string())
    }
}

/// Result of handling one JSON-RPC request.
#[derive(Debug, Clone)]
pub struct RequestOutcome {
    /// Response to send to the client.
    pub response: Response,
    /// Events to replay after the response, for `session.subscribe`.
    pub replay: Vec<EventEnvelope>,
}

impl RequestOutcome {
    fn ok<T: Serialize>(id: Id, result: &T) -> Self {
        Self::ok_with_replay(id, result, Vec::new())
    }

    fn ok_with_replay<T: Serialize>(id: Id, result: &T, replay: Vec<EventEnvelope>) -> Self {
        match serde_json::to_value(result) {
            Ok(value) => Self {
                response: Response::success(id, value),
                replay,
            },
            Err(error) => Self::error(
                id,
                ProtocolError::Internal(error.to_string()).to_error_object(),
            ),
        }
    }

    fn error(id: Id, error: ErrorObject) -> Self {
        Self {
            response: Response::error(id, error),
            replay: Vec::new(),
        }
    }
}

struct SessionState {
    turn: Option<TurnState>,
    tools: Arc<ToolRegistry>,
    gate: PermissionGate,
    snapshots: Arc<SnapshotManager>,
}

struct TurnState {
    turn_id: String,
    cancel: CancellationToken,
}

/// The headless runtime shared by the stdio server, the CLI, and tests.
pub struct Runtime {
    storage: StorageHandle,
    providers: Arc<ProviderRegistry>,
    clock: Arc<dyn Clock>,
    ids: Arc<dyn IdGenerator>,
    context: ContextManager,
    snapshot_base: Option<PathBuf>,
    checkpoint_base: Option<PathBuf>,
    max_turn_cost_usd: f64,
    lsp_config: Option<Arc<LspConfig>>,
    mcp_config: Option<Arc<McpConfig>>,
    rebuild_threshold_tokens: usize,
    history_tail_turns: usize,
    verifier: Option<Arc<Verifier>>,
    sessions: Mutex<HashMap<String, SessionState>>,
    approvals: Mutex<HashMap<String, oneshot::Sender<ApprovalDecision>>>,
    events: broadcast::Sender<EventEnvelope>,
}

impl Runtime {
    /// Build a runtime.
    #[must_use]
    pub fn new(
        storage: StorageHandle,
        provider: Arc<dyn Provider>,
        clock: Arc<dyn Clock>,
        ids: Arc<dyn IdGenerator>,
        context: ContextManager,
    ) -> Self {
        let (events, _) = broadcast::channel(1024);
        Self {
            storage,
            providers: Arc::new(ProviderRegistry::single(provider)),
            clock,
            ids,
            context,
            snapshot_base: None,
            checkpoint_base: None,
            max_turn_cost_usd: DEFAULT_MAX_TURN_COST_USD,
            lsp_config: None,
            mcp_config: None,
            rebuild_threshold_tokens: DEFAULT_REBUILD_THRESHOLD_TOKENS,
            history_tail_turns: DEFAULT_HISTORY_TAIL_TURNS,
            verifier: None,
            sessions: Mutex::new(HashMap::new()),
            approvals: Mutex::new(HashMap::new()),
            events,
        }
    }

    /// Build a runtime backed by a provider registry.
    #[must_use]
    pub fn with_registry(
        storage: StorageHandle,
        providers: Arc<ProviderRegistry>,
        clock: Arc<dyn Clock>,
        ids: Arc<dyn IdGenerator>,
        context: ContextManager,
    ) -> Self {
        let (events, _) = broadcast::channel(1024);
        Self {
            storage,
            providers,
            clock,
            ids,
            context,
            snapshot_base: None,
            checkpoint_base: None,
            max_turn_cost_usd: DEFAULT_MAX_TURN_COST_USD,
            lsp_config: None,
            mcp_config: None,
            rebuild_threshold_tokens: DEFAULT_REBUILD_THRESHOLD_TOKENS,
            history_tail_turns: DEFAULT_HISTORY_TAIL_TURNS,
            verifier: None,
            sessions: Mutex::new(HashMap::new()),
            approvals: Mutex::new(HashMap::new()),
            events,
        }
    }

    /// Store snapshots under the supplied base directory.
    #[must_use]
    pub fn with_snapshot_base(mut self, base: impl Into<PathBuf>) -> Self {
        self.snapshot_base = Some(base.into());
        self
    }

    /// Attach a verifier.
    #[must_use]
    pub fn with_verifier(mut self, verifier: Verifier) -> Self {
        self.verifier = Some(Arc::new(verifier));
        self
    }

    /// Store checkpoints under the supplied base directory.
    #[must_use]
    pub fn with_checkpoint_base(mut self, base: impl Into<PathBuf>) -> Self {
        self.checkpoint_base = Some(base.into());
        self
    }

    /// Set the per-turn cost budget in USD.
    #[must_use]
    pub fn with_cost_limit(mut self, limit: f64) -> Self {
        self.max_turn_cost_usd = limit.max(0.0);
        self
    }

    /// Attach an LSP configuration to session tool registries.
    #[must_use]
    pub fn with_lsp(mut self, config: LspConfig) -> Self {
        self.lsp_config = Some(Arc::new(config));
        self
    }

    /// Attach a stdio MCP configuration to session tool registries.
    #[must_use]
    pub fn with_mcp(mut self, config: McpConfig) -> Self {
        self.mcp_config = Some(Arc::new(config));
        self
    }

    /// Set the prompt token threshold that triggers a context rebuild.
    #[must_use]
    pub fn with_rebuild_threshold(mut self, tokens: usize) -> Self {
        self.rebuild_threshold_tokens = tokens.max(1);
        self
    }

    /// Set how many recent turns stay verbatim across a rebuild.
    #[must_use]
    pub fn with_history_tail_turns(mut self, turns: usize) -> Self {
        self.history_tail_turns = turns.max(1);
        self
    }

    /// Subscribe to every live event emitted by this runtime.
    #[must_use]
    pub fn subscribe_live(&self) -> broadcast::Receiver<EventEnvelope> {
        self.events.subscribe()
    }

    /// Create a session and emit `session_created`.
    pub async fn create_session(
        &self,
        workspace: impl Into<String>,
        mode: SessionMode,
        model: Option<String>,
    ) -> Result<SessionInfo, RuntimeError> {
        self.create_session_with(workspace, mode, model, None, false)
            .await
    }

    /// Create a named session, optionally stealing a workspace lease.
    pub async fn create_session_with(
        &self,
        workspace: impl Into<String>,
        mode: SessionMode,
        model: Option<String>,
        name: Option<String>,
        steal_lease: bool,
    ) -> Result<SessionInfo, RuntimeError> {
        self.create_session_with_parent(workspace, mode, model, name, steal_lease, None)
            .await
    }

    /// Create a session with an optional parent id.
    pub async fn create_session_with_parent(
        &self,
        workspace: impl Into<String>,
        mode: SessionMode,
        model: Option<String>,
        name: Option<String>,
        steal_lease: bool,
        parent_id: Option<String>,
    ) -> Result<SessionInfo, RuntimeError> {
        let workspace = workspace.into();
        let canonical = thais_storage::canonical_workspace(Path::new(&workspace))
            .map_err(|error| RuntimeError::InvalidRequest(format!("invalid workspace: {error}")))?;
        let canonical = canonical
            .to_str()
            .ok_or(RuntimeError::InvalidWorkspace)?
            .to_owned();
        let model = match model {
            Some(model) => model,
            None => self
                .providers
                .models()
                .await?
                .first()
                .map(|model| model.id.clone())
                .or_else(|| self.providers.default_model().map(ToOwned::to_owned))
                .unwrap_or_else(|| "fake-model".to_owned()),
        };
        let session_id = self.ids.new_id("session");
        let timestamp = self.clock.now_rfc3339();
        let name = name.unwrap_or_else(|| {
            Path::new(&canonical)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("session")
                .to_owned()
        });

        if mode == SessionMode::Build
            && let Some(holder) = self
                .storage
                .acquire_lease(
                    canonical.clone(),
                    session_id.clone(),
                    LEASE_TTL_SECS,
                    steal_lease,
                )
                .await?
        {
            return Err(RuntimeError::LeaseHeld(holder.session_id));
        }

        let info = SessionInfo {
            session_id: session_id.clone(),
            parent_id,
            name,
            workspace: canonical,
            mode,
            model: model.clone(),
            model_policy: json!({"default": model}),
            input_tokens: 0,
            output_tokens: 0,
            cost_usd: 0.0,
            status: SessionStatus::Active,
            created_at: timestamp.clone(),
            updated_at: timestamp,
        };

        if let Err(error) = self.storage.create_session(info.clone()).await {
            if info.mode == SessionMode::Build {
                let _ = self
                    .storage
                    .release_lease(info.workspace.clone(), session_id.clone())
                    .await;
            }
            return Err(error.into());
        }
        let tools = self.session_state(&info)?;
        self.sessions.lock().await.insert(session_id.clone(), tools);
        let checkpoint_path = self.checkpoint_path(&session_id);
        write_checkpoint(
            &checkpoint_path,
            &CheckpointState {
                session_id: session_id.clone(),
                name: info.name.clone(),
                workspace: info.workspace.clone(),
                mode: info.mode,
                model_policy: info.model_policy.clone(),
                state: "active".to_owned(),
                plan: None,
                tasks: Vec::new(),
                summary: None,
                last_compacted_seq: 0,
                updated_at: self.clock.now_rfc3339(),
            },
        )?;
        self.append_event(
            &session_id,
            EventKind::SessionCreated,
            json!({"session": &info}),
        )
        .await?;
        Ok(info)
    }

    fn session_state(&self, info: &SessionInfo) -> Result<SessionState, RuntimeError> {
        let tools = match info.mode {
            SessionMode::Plan => ToolRegistry::read_only(Path::new(&info.workspace))?,
            SessionMode::Build => ToolRegistry::read_write(Path::new(&info.workspace))?,
        };
        let tools = if let Some(config) = &self.lsp_config {
            tools.with_lsp((**config).clone())
        } else {
            tools
        };
        let tools = if let Some(config) = &self.mcp_config {
            tools.with_mcp((**config).clone())
        } else {
            tools
        };
        let gate = PermissionGate::new(match info.mode {
            SessionMode::Plan => PolicyMode::Plan,
            SessionMode::Build => PolicyMode::Build,
        });
        let snapshot_root = self.snapshot_base.clone().unwrap_or_else(|| {
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            info.workspace.hash(&mut hasher);
            std::env::temp_dir().join(format!("thais-snapshots-{:016x}", hasher.finish()))
        });
        let snapshots = SnapshotManager::new(
            Path::new(&info.workspace),
            &snapshot_root.join(&info.session_id),
        )?;
        Ok(SessionState {
            turn: None,
            tools: Arc::new(tools),
            gate,
            snapshots: Arc::new(snapshots),
        })
    }

    fn checkpoint_path(&self, session_id: &str) -> PathBuf {
        let base = self
            .checkpoint_base
            .clone()
            .unwrap_or_else(|| std::env::temp_dir().join("thais-sessions"));
        base.join(session_id).join("checkpoint.md")
    }

    /// Fetch session metadata.
    pub async fn get_session(&self, session_id: &str) -> Result<SessionInfo, RuntimeError> {
        Ok(self.storage.get_session(session_id.to_owned()).await?)
    }

    /// List session metadata.
    pub async fn list_sessions(&self) -> Result<Vec<SessionInfo>, RuntimeError> {
        Ok(self.storage.list_sessions().await?)
    }

    /// List sessions, optionally including archived ones.
    pub async fn list_sessions_with_archived(
        &self,
        include_archived: bool,
    ) -> Result<Vec<SessionInfo>, RuntimeError> {
        Ok(self
            .storage
            .list_sessions_with_archived(include_archived)
            .await?)
    }

    /// Rename a session and persist its checkpoint.
    pub async fn rename_session(
        &self,
        session_id: &str,
        name: &str,
    ) -> Result<SessionInfo, RuntimeError> {
        let session = self
            .storage
            .rename_session(session_id.to_owned(), name.to_owned())
            .await?;
        self.persist_checkpoint(session_id, "active", None, Vec::new())
            .await?;
        Ok(session)
    }

    /// Archive a session and release its lease.
    pub async fn archive_session(&self, session_id: &str) -> Result<SessionInfo, RuntimeError> {
        let session = self.storage.archive_session(session_id.to_owned()).await?;
        if session.mode == SessionMode::Build {
            let _ = self
                .storage
                .release_lease(session.workspace.clone(), session_id.to_owned())
                .await;
        }
        self.persist_checkpoint(session_id, "archived", None, Vec::new())
            .await?;
        self.sessions.lock().await.remove(session_id);
        Ok(session)
    }

    /// Release a session's mutating lease without closing the session.
    pub async fn release_session_lease(&self, session_id: &str) -> Result<bool, RuntimeError> {
        let session = self.storage.get_session(session_id.to_owned()).await?;
        if session.mode != SessionMode::Build {
            return Ok(false);
        }
        Ok(self
            .storage
            .release_lease(session.workspace, session_id.to_owned())
            .await?)
    }

    /// Soft-delete a session and release its lease.
    pub async fn delete_session(&self, session_id: &str) -> Result<bool, RuntimeError> {
        let session = self.storage.get_session(session_id.to_owned()).await?;
        let deleted = self.storage.delete_session(session_id.to_owned()).await?;
        if session.mode == SessionMode::Build {
            let _ = self
                .storage
                .release_lease(session.workspace.clone(), session_id.to_owned())
                .await;
        }
        self.sessions.lock().await.remove(session_id);
        Ok(deleted)
    }

    /// Resume a persisted session, rehydrating runtime state and pending approvals.
    pub async fn resume_session(
        &self,
        session_id: &str,
    ) -> Result<SessionResumeResult, RuntimeError> {
        let session = self.storage.get_session(session_id.to_owned()).await?;
        if session.status == SessionStatus::Deleted {
            return Err(RuntimeError::SessionNotFound(session_id.to_owned()));
        }
        if session.mode == SessionMode::Build
            && let Some(holder) = self
                .storage
                .acquire_lease(
                    session.workspace.clone(),
                    session_id.to_owned(),
                    LEASE_TTL_SECS,
                    false,
                )
                .await?
        {
            return Err(RuntimeError::LeaseHeld(holder.session_id));
        }
        if !self.sessions.lock().await.contains_key(session_id) {
            let state = self.session_state(&session)?;
            self.sessions
                .lock()
                .await
                .insert(session_id.to_owned(), state);
        }
        let checkpoint =
            read_checkpoint(&self.checkpoint_path(session_id))?.map(|state| state.summary());
        let pending_approvals = self
            .storage
            .pending_approvals(session_id.to_owned())
            .await?;
        Ok(SessionResumeResult {
            session,
            checkpoint,
            pending_approvals,
        })
    }

    /// Change a role's model in the session policy.
    pub async fn set_model(
        &self,
        session_id: &str,
        role: &str,
        model: &str,
    ) -> Result<SessionInfo, RuntimeError> {
        if !matches!(
            role,
            "default" | "plan" | "build" | "explore" | "general" | "compact"
        ) {
            return Err(RuntimeError::InvalidRequest(format!(
                "unknown model role: {role}"
            )));
        }
        let session = self.storage.get_session(session_id.to_owned()).await?;
        let mut policy = session.model_policy.clone();
        if !policy.is_object() {
            policy = json!({});
        }
        if role == "default" {
            policy["default"] = json!(model);
        } else {
            if !policy
                .get("roles")
                .is_some_and(serde_json::Value::is_object)
            {
                policy["roles"] = json!({});
            }
            policy["roles"][role] = json!([model]);
        }
        let session = self
            .storage
            .update_session_model_policy(session_id.to_owned(), policy)
            .await?;
        let session = if role == "default" {
            self.storage
                .update_session_model(session_id.to_owned(), model.to_owned())
                .await?
        } else {
            session
        };
        self.append_event(
            session_id,
            EventKind::ModelChanged,
            json!({"role": role, "model": model}),
        )
        .await?;
        self.persist_checkpoint(session_id, "active", None, Vec::new())
            .await?;
        Ok(session)
    }

    /// Search the append-only Event Log with optional session/workspace filters.
    pub async fn history_search(
        &self,
        params: HistorySearchParams,
    ) -> Result<HistorySearchResult, RuntimeError> {
        let workspace = match params.workspace {
            Some(workspace) => {
                let canonical =
                    thais_storage::canonical_workspace(Path::new(&workspace)).map_err(|error| {
                        RuntimeError::InvalidRequest(format!("invalid workspace: {error}"))
                    })?;
                Some(
                    canonical
                        .to_str()
                        .ok_or(RuntimeError::InvalidWorkspace)?
                        .to_owned(),
                )
            }
            None => None,
        };
        let hits = self
            .storage
            .search_filtered(
                params.query,
                params.session_id,
                workspace,
                params.limit.unwrap_or(50),
            )
            .await?;
        Ok(HistorySearchResult {
            matches: hits
                .into_iter()
                .map(|hit| HistoryMatch {
                    event: hit.event,
                    session_name: hit.session_name,
                    workspace: hit.workspace,
                })
                .collect(),
        })
    }

    /// List models exposed by the active provider.
    pub async fn list_models(&self) -> Result<Vec<ModelInfo>, RuntimeError> {
        let models = self.providers.models().await?;
        Ok(models.into_iter().map(model_info).collect())
    }

    async fn model_candidates(
        &self,
        session: &SessionInfo,
        role: &str,
        model_override: Option<&str>,
    ) -> Result<Vec<String>, RuntimeError> {
        let policy = ModelPolicy::from_value(&session.model_policy);
        let requested =
            model_override.map_or_else(|| policy.candidates(role), |model| vec![model.to_owned()]);
        let catalog = self.providers.models().await?;
        let requested_first = requested.first().cloned();
        let mut candidates: Vec<String> = requested
            .into_iter()
            .filter(|model| {
                self.providers.provider_for(model).is_some()
                    && catalog
                        .iter()
                        .find(|candidate| &candidate.id == model)
                        .is_none_or(|candidate| candidate.supports_tools)
            })
            .collect();
        candidates.dedup();
        if candidates.is_empty() && model_override.is_none() {
            candidates = catalog
                .iter()
                .filter(|model| model.supports_tools)
                .map(|model| model.id.clone())
                .collect();
            candidates.dedup();
        }
        if candidates.is_empty() {
            return Err(RuntimeError::InvalidRequest(if model_override.is_some() {
                "the override model is unavailable or does not support tool calling".to_owned()
            } else {
                "no model with tool calling is available".to_owned()
            }));
        }
        if let Some(first) = requested_first
            && candidates.first() != Some(&first)
        {
            self.append_event(
                &session.session_id,
                EventKind::ModelChanged,
                json!({"role": role, "model": candidates[0], "reason": "fallback"}),
            )
            .await?;
        }
        Ok(candidates)
    }

    async fn persist_checkpoint(
        &self,
        session_id: &str,
        state: &str,
        plan: Option<String>,
        tasks: Vec<String>,
    ) -> Result<(), RuntimeError> {
        let session = self.storage.get_session(session_id.to_owned()).await?;
        let path = self.checkpoint_path(session_id);
        let previous = read_checkpoint(&path)?;
        let plan = plan.or_else(|| previous.as_ref().and_then(|state| state.plan.clone()));
        let tasks = if tasks.is_empty() {
            previous
                .as_ref()
                .map_or_else(Vec::new, |state| state.tasks.clone())
        } else {
            tasks
        };
        let summary = previous.as_ref().and_then(|state| state.summary.clone());
        let last_compacted_seq = previous
            .as_ref()
            .map_or(0, |state| state.last_compacted_seq);
        let checkpoint = CheckpointState {
            session_id: session.session_id,
            name: session.name,
            workspace: session.workspace,
            mode: session.mode,
            model_policy: session.model_policy,
            state: state.to_owned(),
            plan,
            tasks,
            summary,
            last_compacted_seq,
            updated_at: self.clock.now_rfc3339(),
        };
        write_checkpoint(&path, &checkpoint)?;
        Ok(())
    }

    /// Rebuild a session view from the Event Log.
    pub async fn replay_session(&self, session_id: &str) -> Result<ReplayState, RuntimeError> {
        Ok(crate::replay::replay_session(&self.storage, session_id).await?)
    }

    /// Start a turn. The turn runs in a background task and emits events.
    pub async fn send(
        self: &Arc<Self>,
        session_id: &str,
        input: String,
        turn_id: Option<String>,
    ) -> Result<String, RuntimeError> {
        self.send_with_model(session_id, input, turn_id, None).await
    }

    /// Start a turn with an optional per-turn model override.
    pub async fn send_with_model(
        self: &Arc<Self>,
        session_id: &str,
        input: String,
        turn_id: Option<String>,
        model: Option<String>,
    ) -> Result<String, RuntimeError> {
        let session_info = self.storage.get_session(session_id.to_owned()).await?;
        if session_info.status != SessionStatus::Active {
            return Err(RuntimeError::InvalidRequest(format!(
                "session {} is {}",
                session_id,
                session_info.status.as_str()
            )));
        }
        if session_info.mode == SessionMode::Build {
            let renewed = self
                .storage
                .renew_lease(
                    session_info.workspace.clone(),
                    session_id.to_owned(),
                    LEASE_TTL_SECS,
                )
                .await?;
            if !renewed
                && let Some(holder) = self
                    .storage
                    .acquire_lease(
                        session_info.workspace.clone(),
                        session_id.to_owned(),
                        LEASE_TTL_SECS,
                        false,
                    )
                    .await?
            {
                return Err(RuntimeError::LeaseHeld(holder.session_id));
            }
        }
        let turn_id = turn_id.unwrap_or_else(|| self.ids.new_id("turn"));
        let cancel = CancellationToken::new();

        {
            let mut sessions = self.sessions.lock().await;
            let state = sessions
                .get_mut(session_id)
                .ok_or_else(|| RuntimeError::SessionNotFound(session_id.to_owned()))?;
            if let Some(turn) = &state.turn {
                return Err(RuntimeError::TurnInProgress(turn.turn_id.clone()));
            }
            state.turn = Some(TurnState {
                turn_id: turn_id.clone(),
                cancel: cancel.clone(),
            });
        }

        let (tools, gate, snapshots) = {
            let sessions = self.sessions.lock().await;
            let state = sessions
                .get(session_id)
                .ok_or_else(|| RuntimeError::SessionNotFound(session_id.to_owned()))?;
            (
                Arc::clone(&state.tools),
                state.gate,
                Arc::clone(&state.snapshots),
            )
        };

        let runtime = Arc::clone(self);
        let session = session_id.to_owned();
        let turn = turn_id.clone();
        tokio::spawn(async move {
            runtime
                .run_turn(session, turn, input, model, cancel, tools, gate, snapshots)
                .await;
        });
        Ok(turn_id)
    }

    /// Cancel the active turn, or a specific turn when `turn_id` is supplied.
    pub async fn cancel(
        &self,
        session_id: &str,
        turn_id: Option<&str>,
    ) -> Result<bool, RuntimeError> {
        let sessions = self.sessions.lock().await;
        let state = sessions
            .get(session_id)
            .ok_or_else(|| RuntimeError::SessionNotFound(session_id.to_owned()))?;
        if let Some(turn) = &state.turn
            && (turn_id.is_none() || turn_id == Some(turn.turn_id.as_str()))
        {
            turn.cancel.cancel();
            return Ok(true);
        }
        Ok(false)
    }

    /// Resolve a pending approval.
    pub async fn approve(
        &self,
        session_id: &str,
        approval_id: &str,
        decision: ApprovalDecision,
    ) -> Result<bool, RuntimeError> {
        self.storage.get_session(session_id.to_owned()).await?;
        let sender = self.approvals.lock().await.remove(approval_id);
        let decision_text = match decision {
            ApprovalDecision::Allow => "allow",
            ApprovalDecision::Deny => "deny",
        };
        let resolved = self
            .storage
            .resolve_approval(approval_id.to_owned(), decision_text.to_owned(), None)
            .await?;
        if let Some(sender) = sender {
            let _ = sender.send(decision);
            self.append_event(
                session_id,
                EventKind::PermissionResolved,
                json!({"approval_id": approval_id, "decision": decision}),
            )
            .await?;
            return Ok(true);
        }
        if resolved {
            self.append_event(
                session_id,
                EventKind::PermissionResolved,
                json!({"approval_id": approval_id, "decision": decision}),
            )
            .await?;
            return Ok(true);
        }
        Ok(false)
    }

    /// Request approval for an action and wait for a decision.
    pub async fn request_approval(
        &self,
        session_id: &str,
        action: Value,
    ) -> Result<ApprovalDecision, RuntimeError> {
        self.storage.get_session(session_id.to_owned()).await?;
        let approval_id = self.ids.new_id("approval");
        let (sender, receiver) = oneshot::channel();
        self.approvals
            .lock()
            .await
            .insert(approval_id.clone(), sender);
        self.storage
            .insert_pending_approval(approval_id.clone(), session_id.to_owned(), action.clone())
            .await?;
        self.append_event(
            session_id,
            EventKind::PermissionRequested,
            json!({"approval_id": approval_id.clone(), "action": action}),
        )
        .await?;

        match tokio::time::timeout(APPROVAL_TIMEOUT, receiver).await {
            Ok(Ok(decision)) => Ok(decision),
            Ok(Err(_)) => {
                let _ = self
                    .storage
                    .resolve_approval(
                        approval_id.clone(),
                        "deny".to_owned(),
                        Some("approval channel closed".to_owned()),
                    )
                    .await;
                Err(RuntimeError::ApprovalNotFound(approval_id))
            }
            Err(_) => {
                self.approvals.lock().await.remove(&approval_id);
                let _ = self
                    .storage
                    .resolve_approval(
                        approval_id.clone(),
                        "deny".to_owned(),
                        Some("approval timeout".to_owned()),
                    )
                    .await;
                self.append_event(
                    session_id,
                    EventKind::PermissionResolved,
                    json!({
                        "approval_id": approval_id,
                        "decision": "deny",
                        "reason": "approval timeout"
                    }),
                )
                .await?;
                Ok(ApprovalDecision::Deny)
            }
        }
    }

    /// Subscribe to a session, optionally replaying events after `from_seq`.
    pub async fn subscribe(
        &self,
        session_id: &str,
        from_seq: Option<u64>,
    ) -> Result<(String, u64, Vec<EventEnvelope>), RuntimeError> {
        self.storage.get_session(session_id.to_owned()).await?;
        let last = self.storage.last_seq(session_id.to_owned()).await?;
        let after = from_seq.unwrap_or(last);
        let replay = self
            .storage
            .events_after(session_id.to_owned(), after)
            .await?;
        let first = replay.first().map_or(after + 1, |event| event.seq);
        let subscription_id = self.ids.new_id("sub");
        Ok((subscription_id, first, replay))
    }

    /// Run one turn to completion and return every event observed.
    pub async fn run_once(
        self: &Arc<Self>,
        workspace: String,
        task: String,
        model: Option<String>,
    ) -> Result<(SessionInfo, String, Vec<EventEnvelope>), RuntimeError> {
        self.run_once_mode(workspace, task, model, SessionMode::Build)
            .await
    }

    /// Run one turn in a specific mode.
    pub async fn run_once_mode(
        self: &Arc<Self>,
        workspace: String,
        task: String,
        model: Option<String>,
        mode: SessionMode,
    ) -> Result<(SessionInfo, String, Vec<EventEnvelope>), RuntimeError> {
        let mut live = self.subscribe_live();
        let session = self.create_session(workspace, mode, model).await?;
        let turn_id = self.send(&session.session_id, task, None).await?;

        let mut events = Vec::new();
        loop {
            match live.recv().await {
                Ok(event) if event.session_id == session.session_id => {
                    let terminal = matches!(
                        event.kind,
                        EventKind::TurnCompleted | EventKind::TurnCancelled | EventKind::Error
                    );
                    events.push(event);
                    if terminal {
                        break;
                    }
                }
                Ok(_) => {}
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
        if session.mode == SessionMode::Build {
            let _ = self
                .storage
                .release_lease(session.workspace.clone(), session.session_id.clone())
                .await;
        }
        Ok((session, turn_id, events))
    }

    /// Handle one JSON-RPC request.
    pub async fn handle_request(self: &Arc<Self>, request: Request) -> RequestOutcome {
        let id = request.id.clone();
        if let Err(error) = request.validate() {
            return RequestOutcome::error(id, error.to_error_object());
        }

        match request.method.as_str() {
            method::INITIALIZE => {
                let params: InitializeParams = match request.params_as() {
                    Ok(params) => params,
                    Err(error) => {
                        return RequestOutcome::error(
                            id,
                            ProtocolError::InvalidParams(error.to_string()).to_error_object(),
                        );
                    }
                };
                if !params.protocol_version.is_empty()
                    && params.protocol_version != PROTOCOL_VERSION
                {
                    return RequestOutcome::error(
                        id,
                        ProtocolError::InvalidVersion(params.protocol_version).to_error_object(),
                    );
                }
                let result = InitializeResult {
                    server_name: "thais".to_owned(),
                    server_version: env!("CARGO_PKG_VERSION").to_owned(),
                    protocol_version: PROTOCOL_VERSION.to_owned(),
                    capabilities: json!({
                        "sessions": true,
                        "approvals": true,
                        "fake_provider": true
                    }),
                };
                RequestOutcome::ok(id, &result)
            }
            method::SESSION_CREATE => {
                let params: SessionCreateParams = match request.params_as() {
                    Ok(params) => params,
                    Err(error) => {
                        return RequestOutcome::error(
                            id,
                            ProtocolError::InvalidParams(error.to_string()).to_error_object(),
                        );
                    }
                };
                match self
                    .create_session_with_parent(
                        params.workspace,
                        params.mode,
                        params.model,
                        params.name,
                        params.steal_lease,
                        params.parent_id,
                    )
                    .await
                {
                    Ok(session) => RequestOutcome::ok(id, &SessionCreateResult { session }),
                    Err(error) => RequestOutcome::error(id, error.to_error_object()),
                }
            }
            method::SESSION_SEND => {
                let params: SessionSendParams = match request.params_as() {
                    Ok(params) => params,
                    Err(error) => {
                        return RequestOutcome::error(
                            id,
                            ProtocolError::InvalidParams(error.to_string()).to_error_object(),
                        );
                    }
                };
                match self
                    .send_with_model(
                        &params.session_id,
                        params.input,
                        params.turn_id,
                        params.model,
                    )
                    .await
                {
                    Ok(turn_id) => RequestOutcome::ok(id, &SessionSendResult { turn_id }),
                    Err(error) => RequestOutcome::error(id, error.to_error_object()),
                }
            }
            method::SESSION_CANCEL => {
                let params: SessionCancelParams = match request.params_as() {
                    Ok(params) => params,
                    Err(error) => {
                        return RequestOutcome::error(
                            id,
                            ProtocolError::InvalidParams(error.to_string()).to_error_object(),
                        );
                    }
                };
                match self
                    .cancel(&params.session_id, params.turn_id.as_deref())
                    .await
                {
                    Ok(cancelled) => RequestOutcome::ok(id, &SessionCancelResult { cancelled }),
                    Err(error) => RequestOutcome::error(id, error.to_error_object()),
                }
            }
            method::SESSION_APPROVE => {
                let params: SessionApproveParams = match request.params_as() {
                    Ok(params) => params,
                    Err(error) => {
                        return RequestOutcome::error(
                            id,
                            ProtocolError::InvalidParams(error.to_string()).to_error_object(),
                        );
                    }
                };
                match self
                    .approve(&params.session_id, &params.approval_id, params.decision)
                    .await
                {
                    Ok(resolved) => RequestOutcome::ok(id, &SessionApproveResult { resolved }),
                    Err(error) => RequestOutcome::error(id, error.to_error_object()),
                }
            }
            method::SESSION_SUBSCRIBE => {
                let params: SessionSubscribeParams = match request.params_as() {
                    Ok(params) => params,
                    Err(error) => {
                        return RequestOutcome::error(
                            id,
                            ProtocolError::InvalidParams(error.to_string()).to_error_object(),
                        );
                    }
                };
                match self.subscribe(&params.session_id, params.from_seq).await {
                    Ok((subscription_id, from_seq, replay)) => RequestOutcome::ok_with_replay(
                        id,
                        &SessionSubscribeResult {
                            subscription_id,
                            from_seq,
                        },
                        replay,
                    ),
                    Err(error) => RequestOutcome::error(id, error.to_error_object()),
                }
            }
            method::SESSION_LIST => {
                let params: SessionListParams = if request.params.is_null() {
                    SessionListParams::default()
                } else {
                    match request.params_as() {
                        Ok(params) => params,
                        Err(error) => {
                            return RequestOutcome::error(
                                id,
                                ProtocolError::InvalidParams(error.to_string()).to_error_object(),
                            );
                        }
                    }
                };
                match self
                    .list_sessions_with_archived(params.include_archived)
                    .await
                {
                    Ok(sessions) => RequestOutcome::ok(id, &SessionListResult { sessions }),
                    Err(error) => RequestOutcome::error(id, error.to_error_object()),
                }
            }
            method::SESSION_GET => {
                let params: SessionGetParams = match request.params_as() {
                    Ok(params) => params,
                    Err(error) => {
                        return RequestOutcome::error(
                            id,
                            ProtocolError::InvalidParams(error.to_string()).to_error_object(),
                        );
                    }
                };
                match self.get_session(&params.session_id).await {
                    Ok(session) => RequestOutcome::ok(id, &SessionGetResult { session }),
                    Err(error) => RequestOutcome::error(id, error.to_error_object()),
                }
            }
            method::SESSION_RENAME => {
                let params: SessionRenameParams = match request.params_as() {
                    Ok(params) => params,
                    Err(error) => {
                        return RequestOutcome::error(
                            id,
                            ProtocolError::InvalidParams(error.to_string()).to_error_object(),
                        );
                    }
                };
                match self.rename_session(&params.session_id, &params.name).await {
                    Ok(session) => RequestOutcome::ok(id, &SessionRenameResult { session }),
                    Err(error) => RequestOutcome::error(id, error.to_error_object()),
                }
            }
            method::SESSION_ARCHIVE => {
                let params: SessionArchiveParams = match request.params_as() {
                    Ok(params) => params,
                    Err(error) => {
                        return RequestOutcome::error(
                            id,
                            ProtocolError::InvalidParams(error.to_string()).to_error_object(),
                        );
                    }
                };
                match self.archive_session(&params.session_id).await {
                    Ok(session) => RequestOutcome::ok(id, &SessionArchiveResult { session }),
                    Err(error) => RequestOutcome::error(id, error.to_error_object()),
                }
            }
            method::SESSION_RELEASE_LEASE => {
                let params: SessionReleaseLeaseParams = match request.params_as() {
                    Ok(params) => params,
                    Err(error) => {
                        return RequestOutcome::error(
                            id,
                            ProtocolError::InvalidParams(error.to_string()).to_error_object(),
                        );
                    }
                };
                match self.release_session_lease(&params.session_id).await {
                    Ok(released) => RequestOutcome::ok(id, &SessionReleaseLeaseResult { released }),
                    Err(error) => RequestOutcome::error(id, error.to_error_object()),
                }
            }
            method::SESSION_DELETE => {
                let params: SessionDeleteParams = match request.params_as() {
                    Ok(params) => params,
                    Err(error) => {
                        return RequestOutcome::error(
                            id,
                            ProtocolError::InvalidParams(error.to_string()).to_error_object(),
                        );
                    }
                };
                match self.delete_session(&params.session_id).await {
                    Ok(deleted) => RequestOutcome::ok(id, &SessionDeleteResult { deleted }),
                    Err(error) => RequestOutcome::error(id, error.to_error_object()),
                }
            }
            method::SESSION_RESUME => {
                let params: SessionResumeParams = match request.params_as() {
                    Ok(params) => params,
                    Err(error) => {
                        return RequestOutcome::error(
                            id,
                            ProtocolError::InvalidParams(error.to_string()).to_error_object(),
                        );
                    }
                };
                match self.resume_session(&params.session_id).await {
                    Ok(result) => RequestOutcome::ok(id, &result),
                    Err(error) => RequestOutcome::error(id, error.to_error_object()),
                }
            }
            method::SESSION_SET_MODEL => {
                let params: SessionSetModelParams = match request.params_as() {
                    Ok(params) => params,
                    Err(error) => {
                        return RequestOutcome::error(
                            id,
                            ProtocolError::InvalidParams(error.to_string()).to_error_object(),
                        );
                    }
                };
                match self
                    .set_model(&params.session_id, &params.role, &params.model)
                    .await
                {
                    Ok(session) => RequestOutcome::ok(id, &SessionSetModelResult { session }),
                    Err(error) => RequestOutcome::error(id, error.to_error_object()),
                }
            }
            method::HISTORY_SEARCH => {
                let params: HistorySearchParams = match request.params_as() {
                    Ok(params) => params,
                    Err(error) => {
                        return RequestOutcome::error(
                            id,
                            ProtocolError::InvalidParams(error.to_string()).to_error_object(),
                        );
                    }
                };
                match self.history_search(params).await {
                    Ok(result) => RequestOutcome::ok(id, &result),
                    Err(error) => RequestOutcome::error(id, error.to_error_object()),
                }
            }
            method::MODELS_LIST => match self.list_models().await {
                Ok(models) => RequestOutcome::ok(id, &ModelsListResult { models }),
                Err(error) => RequestOutcome::error(id, error.to_error_object()),
            },
            other => RequestOutcome::error(
                id,
                ProtocolError::MethodNotFound(other.to_owned()).to_error_object(),
            ),
        }
    }

    async fn append_event(
        &self,
        session_id: &str,
        kind: EventKind,
        payload: Value,
    ) -> Result<EventEnvelope, RuntimeError> {
        let event = NewEvent::new(
            self.ids.new_id("evt"),
            session_id,
            self.clock.now_rfc3339(),
            kind,
            payload,
        );
        let event = self.storage.append_event(event).await?;
        let _ = self.events.send(event.clone());
        Ok(event)
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_turn(
        self: Arc<Self>,
        session_id: String,
        turn_id: String,
        input: String,
        model: Option<String>,
        cancel: CancellationToken,
        tools: Arc<ToolRegistry>,
        gate: PermissionGate,
        snapshots: Arc<SnapshotManager>,
    ) {
        if let Err(error) = self
            .run_turn_inner(
                &session_id,
                &turn_id,
                &input,
                model.as_deref(),
                &cancel,
                tools,
                gate,
                snapshots,
            )
            .await
        {
            let _ = self
                .append_event(
                    &session_id,
                    EventKind::Error,
                    json!({"turn_id": turn_id, "message": error.to_string()}),
                )
                .await;
        }

        let mut sessions = self.sessions.lock().await;
        if let Some(state) = sessions.get_mut(&session_id)
            && state
                .turn
                .as_ref()
                .is_some_and(|turn| turn.turn_id == turn_id)
        {
            state.turn = None;
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_subagent(
        &self,
        parent: &SessionInfo,
        kind: &str,
        task: &str,
        cancel: &CancellationToken,
        parent_turn_id: &str,
    ) -> Result<ToolResult, ToolError> {
        if task.trim().is_empty() {
            return Err(ToolError::InvalidArgs("subagent task is empty".to_owned()));
        }
        if !matches!(kind, "explore" | "general") {
            return Err(ToolError::InvalidArgs(format!(
                "unknown subagent kind: {kind}"
            )));
        }
        if parent.mode == SessionMode::Plan && kind == "general" {
            return Err(ToolError::Denied(
                "general subagents require build mode".to_owned(),
            ));
        }
        if kind == "general" {
            let decision = self
                .request_approval(
                    &parent.session_id,
                    json!({
                        "type": "subagent",
                        "kind": kind,
                        "task": task,
                        "parent_turn_id": parent_turn_id
                    }),
                )
                .await
                .map_err(|error| ToolError::Command(error.to_string()))?;
            if decision != ApprovalDecision::Allow {
                return Err(ToolError::Denied("subagent denied by user".to_owned()));
            }
        }

        let mode = if kind == "explore" {
            SessionMode::Plan
        } else {
            SessionMode::Build
        };
        let child_id = self.ids.new_id("session");
        let now = self.clock.now_rfc3339();
        let child = SessionInfo {
            session_id: child_id.clone(),
            parent_id: Some(parent.session_id.clone()),
            name: format!("{kind}: {}", truncate_text(task, 60)),
            workspace: parent.workspace.clone(),
            mode,
            model: parent.model.clone(),
            model_policy: parent.model_policy.clone(),
            input_tokens: 0,
            output_tokens: 0,
            cost_usd: 0.0,
            status: SessionStatus::Active,
            created_at: now.clone(),
            updated_at: now,
        };
        self.storage
            .create_session(child.clone())
            .await
            .map_err(|error| ToolError::Command(error.to_string()))?;
        write_checkpoint(
            &self.checkpoint_path(&child_id),
            &CheckpointState {
                session_id: child_id.clone(),
                name: child.name.clone(),
                workspace: child.workspace.clone(),
                mode,
                model_policy: child.model_policy.clone(),
                state: "subagent".to_owned(),
                plan: None,
                tasks: vec![task.to_owned()],
                summary: None,
                last_compacted_seq: 0,
                updated_at: self.clock.now_rfc3339(),
            },
        )
        .map_err(|error| ToolError::Command(error.to_string()))?;
        self.append_event(
            &child_id,
            EventKind::SessionCreated,
            json!({"session": &child, "parent_turn_id": parent_turn_id}),
        )
        .await
        .map_err(|error| ToolError::Command(error.to_string()))?;

        let tools = match mode {
            SessionMode::Plan => ToolRegistry::read_only(Path::new(&child.workspace)),
            SessionMode::Build => ToolRegistry::read_write(Path::new(&child.workspace)),
        }
        .map_err(|error| ToolError::Command(error.to_string()))?
        .without_tool("subagent");
        let tools = if let Some(config) = &self.lsp_config {
            tools.with_lsp((**config).clone())
        } else {
            tools
        };
        let tools = if let Some(config) = &self.mcp_config {
            tools.with_mcp((**config).clone())
        } else {
            tools
        };
        let tools = Arc::new(tools);
        let gate = PermissionGate::new(match mode {
            SessionMode::Plan => PolicyMode::Plan,
            SessionMode::Build => PolicyMode::Build,
        });
        let snapshot_root = self.snapshot_base.clone().unwrap_or_else(|| {
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            child.workspace.hash(&mut hasher);
            std::env::temp_dir().join(format!("thais-snapshots-{:016x}", hasher.finish()))
        });
        let snapshots = Arc::new(
            SnapshotManager::new(Path::new(&child.workspace), &snapshot_root.join(&child_id))
                .map_err(|error| ToolError::Command(error.to_string()))?,
        );
        let role = if mode == SessionMode::Plan {
            "explore"
        } else {
            "build"
        };
        let mut candidates = self
            .model_candidates(&child, role, None)
            .await
            .map_err(|error| ToolError::Command(error.to_string()))?;
        let model = candidates.remove(0);
        let provider = self
            .providers
            .provider_for(&model)
            .ok_or_else(|| ToolError::Command(format!("no provider for model {model}")))?;
        let bundle = self
            .context
            .build(Path::new(&child.workspace), task)
            .await
            .map_err(|error| ToolError::Command(error.to_string()))?;
        let tool_definitions = tools
            .definitions()
            .into_iter()
            .map(|definition| ToolDefinition {
                name: definition.name,
                description: definition.description,
                parameters: definition.parameters,
            })
            .collect::<Vec<_>>();
        let mut messages = vec![
            ChatMessage::system(bundle.system),
            ChatMessage::user(bundle.user),
        ];
        let mut total_tokens = bundle.estimated_tokens as u64;
        let mut diffs = Vec::new();
        let started = tokio::time::Instant::now();
        let turn_id = self.ids.new_id("turn");
        self.append_event(
            &child_id,
            EventKind::TurnStarted,
            json!({"turn_id": turn_id, "input": task, "parent_turn_id": parent_turn_id}),
        )
        .await
        .map_err(|error| ToolError::Command(error.to_string()))?;

        for _ in 0..SUBAGENT_MAX_ITERATIONS {
            if started.elapsed() > SUBAGENT_MAX_TIME {
                return Err(ToolError::Command(
                    "subagent time budget exhausted".to_owned(),
                ));
            }
            if total_tokens > SUBAGENT_MAX_TOKENS {
                return Err(ToolError::Command(
                    "subagent token budget exhausted".to_owned(),
                ));
            }
            let request = ChatRequest {
                model: model.clone(),
                messages: messages.clone(),
                tools: tool_definitions.clone(),
                ..ChatRequest::default()
            };
            let mut stream = provider
                .chat(request)
                .await
                .map_err(|error| ToolError::Command(error.to_string()))?;
            let mut content = String::new();
            let mut accumulators: HashMap<usize, ToolAccumulator> = HashMap::new();
            loop {
                let item = tokio::select! {
                    () = cancel.cancelled() => {
                        let _ = self.append_event(
                            &child_id,
                            EventKind::TurnCancelled,
                            json!({"turn_id": turn_id}),
                        ).await;
                        return Err(ToolError::Command("subagent cancelled".to_owned()));
                    }
                    item = stream.next() => item,
                };
                match item {
                    None => break,
                    Some(Ok(ChatEvent::Content { text })) => {
                        total_tokens = total_tokens.saturating_add(estimate_tokens(&text) as u64);
                        content.push_str(&text);
                        let _ = self
                            .append_event(
                                &child_id,
                                EventKind::ModelDelta,
                                json!({"turn_id": turn_id, "text": text}),
                            )
                            .await;
                    }
                    Some(Ok(ChatEvent::Reasoning { text })) => {
                        total_tokens = total_tokens.saturating_add(estimate_tokens(&text) as u64);
                        let _ = self
                            .append_event(
                                &child_id,
                                EventKind::ReasoningDelta,
                                json!({"turn_id": turn_id, "text": text}),
                            )
                            .await;
                    }
                    Some(Ok(ChatEvent::ToolCallDelta {
                        index,
                        id,
                        name,
                        arguments,
                    })) => {
                        let accumulator = accumulators.entry(index).or_default();
                        if let Some(id) = id {
                            accumulator.id = Some(id);
                        }
                        if let Some(name) = name {
                            accumulator.name = Some(name);
                        }
                        accumulator.arguments.push_str(&arguments);
                    }
                    Some(Ok(ChatEvent::Usage { usage })) => {
                        total_tokens =
                            total_tokens.saturating_add(usage.input_tokens + usage.output_tokens);
                    }
                    Some(Ok(ChatEvent::Done)) => break,
                    Some(Err(error)) => {
                        return Err(ToolError::Command(error.to_string()));
                    }
                }
            }

            let mut calls = Vec::new();
            for (_, accumulator) in accumulators {
                let Some(name) = accumulator.name else {
                    continue;
                };
                let id = accumulator.id.unwrap_or_else(|| self.ids.new_id("call"));
                calls.push(ToolCall {
                    id,
                    name,
                    arguments: accumulator.arguments,
                });
            }
            if calls.is_empty() {
                let _ = self
                    .append_event(
                        &child_id,
                        EventKind::TurnCompleted,
                        json!({
                            "turn_id": turn_id,
                            "parent_turn_id": parent_turn_id,
                            "summary": truncate_text(&content, 4_000)
                        }),
                    )
                    .await;
                let mut output = format!(
                    "subagent {kind} completed

{}",
                    truncate_text(content.trim(), 8_000)
                );
                if !diffs.is_empty() {
                    output.push_str("\n\nDiff:\n");
                    output.push_str(&truncate_text(&diffs.join("\n"), 8_000));
                }
                return Ok(ToolResult {
                    content: output,
                    is_error: false,
                    diff: (!diffs.is_empty()).then(|| diffs.join("\n")),
                });
            }

            let assistant_calls = calls.clone();
            messages.push(ChatMessage::assistant(
                (!content.trim().is_empty()).then_some(content),
                assistant_calls,
            ));
            for call in calls {
                let arguments: Value = serde_json::from_str(&call.arguments)
                    .map_err(|error| ToolError::InvalidArgs(format!("invalid args: {error}")))?;
                self.append_event(
                    &child_id,
                    EventKind::ToolCall,
                    json!({
                        "turn_id": turn_id,
                        "id": call.id,
                        "name": call.name,
                        "arguments": call.arguments
                    }),
                )
                .await
                .map_err(|error| ToolError::Command(error.to_string()))?;
                let decision = gate.check(
                    &call.name,
                    &arguments,
                    tools.jail(),
                    tools.is_known(&call.name),
                    tools.is_mutating(&call.name),
                    &tools.summary(&call.name, &arguments),
                );
                let decision = match decision {
                    PermissionDecision::Allow => ApprovalDecision::Allow,
                    PermissionDecision::Ask(reason) => self
                        .request_approval(
                            &parent.session_id,
                            json!({
                                "type": "subagent_tool",
                                "child_session_id": child_id,
                                "tool": call.name,
                                "arguments": arguments,
                                "reason": reason,
                                "diff": tools.preview(&call.name, &arguments)
                            }),
                        )
                        .await
                        .map_err(|error| ToolError::Command(error.to_string()))?,
                    PermissionDecision::Deny(reason) => {
                        let result = ToolResult {
                            content: format!("denied: {reason}"),
                            is_error: true,
                            diff: None,
                        };
                        self.append_event(
                            &child_id,
                            EventKind::ToolResult,
                            json!({
                                "turn_id": turn_id,
                                "tool_call_id": call.id,
                                "name": call.name,
                                "content": result.content,
                                "is_error": true
                            }),
                        )
                        .await
                        .map_err(|error| ToolError::Command(error.to_string()))?;
                        messages.push(ChatMessage::tool(call.id, result.content.clone()));
                        continue;
                    }
                };
                if decision == ApprovalDecision::Deny {
                    let result = ToolResult {
                        content: "denied by user".to_owned(),
                        is_error: true,
                        diff: None,
                    };
                    self.append_event(
                        &child_id,
                        EventKind::ToolResult,
                        json!({
                            "turn_id": turn_id,
                            "tool_call_id": call.id,
                            "name": call.name,
                            "content": result.content,
                            "is_error": true
                        }),
                    )
                    .await
                    .map_err(|error| ToolError::Command(error.to_string()))?;
                    messages.push(ChatMessage::tool(call.id, result.content.clone()));
                    continue;
                }

                let mut snapshot = None;
                if tools.is_mutating(&call.name) {
                    snapshot = Some(
                        snapshots
                            .create()
                            .map_err(|error| ToolError::Command(error.to_string()))?,
                    );
                }
                let result = tools
                    .execute(&call.name, arguments.clone())
                    .map(redact_tool_result)
                    .map_err(|error| ToolError::Command(error.to_string()))?;
                if snapshot.is_some()
                    && let Some(verifier) = &self.verifier
                {
                    match verifier.run(tools.jail().root()) {
                        Verification::Pass => {}
                        Verification::Fail { output } => {
                            if let Some(snapshot) = &snapshot {
                                let _ = snapshots.restore(snapshot);
                            }
                            let _ = self
                                .append_event(
                                    &child_id,
                                    EventKind::VerificationFailed,
                                    json!({"turn_id": turn_id, "output": truncate_output(&output)}),
                                )
                                .await;
                            return Err(ToolError::Command(
                                "subagent verification failed; changes rolled back".to_owned(),
                            ));
                        }
                        Verification::Unavailable { .. } => {}
                    }
                }
                if let Some(diff) = &result.diff {
                    diffs.push(diff.clone());
                }
                self.append_event(
                    &child_id,
                    EventKind::ToolResult,
                    json!({
                        "turn_id": turn_id,
                        "tool_call_id": call.id,
                        "name": call.name,
                        "content": truncate_output(&result.content),
                        "is_error": result.is_error,
                        "diff": result.diff
                    }),
                )
                .await
                .map_err(|error| ToolError::Command(error.to_string()))?;
                messages.push(ChatMessage::tool(call.id, result.content));
            }
        }
        Err(ToolError::Command(
            "subagent iteration budget exhausted".to_owned(),
        ))
    }

    async fn build_prompt(
        &self,
        session: &SessionInfo,
        input: &str,
        turn_id: &str,
    ) -> Result<crate::context::PromptBundle, RuntimeError> {
        let path = self.checkpoint_path(&session.session_id);
        let checkpoint = read_checkpoint(&path)?;
        let last_seq = checkpoint
            .as_ref()
            .map_or(0, |checkpoint| checkpoint.last_compacted_seq);
        let events = self
            .storage
            .events_after(session.session_id.clone(), last_seq)
            .await?;
        let session_context = format_session_context(checkpoint.as_ref());
        let history = format_history(&events);
        let bundle = self
            .context
            .build_with_history(
                Path::new(&session.workspace),
                input,
                Some(&history),
                Some(&session_context),
            )
            .await?;
        if bundle.estimated_tokens <= self.rebuild_threshold_tokens || events.is_empty() {
            return Ok(bundle);
        }

        // Write-ahead checkpoint before touching the prompt state.
        self.persist_checkpoint(&session.session_id, "rebuilding", None, Vec::new())
            .await?;
        match self
            .rebuild_prompt(session, input, turn_id, checkpoint.as_ref(), &events)
            .await
        {
            Ok(bundle) => Ok(bundle),
            Err(error) => {
                let _ = self
                    .append_event(
                        &session.session_id,
                        EventKind::ContextRebuildFailed,
                        json!({"turn_id": turn_id, "message": error.to_string()}),
                    )
                    .await;
                let fallback_events = tail_turns(&events, self.history_tail_turns);
                let fallback_history = format_history(&fallback_events);
                let session_context = format_session_context(checkpoint.as_ref());
                let bundle = self
                    .context
                    .build_with_history(
                        Path::new(&session.workspace),
                        input,
                        Some(&fallback_history),
                        Some(&session_context),
                    )
                    .await?;
                let _ = self
                    .persist_checkpoint(&session.session_id, "active", None, Vec::new())
                    .await;
                Ok(bundle)
            }
        }
    }

    async fn rebuild_prompt(
        &self,
        session: &SessionInfo,
        input: &str,
        turn_id: &str,
        checkpoint: Option<&CheckpointState>,
        events: &[EventEnvelope],
    ) -> Result<crate::context::PromptBundle, RuntimeError> {
        let turns = split_turns(events);
        if turns.len() < 2 {
            return Err(RuntimeError::InvalidRequest(
                "not enough turns to compact".to_owned(),
            ));
        }
        let keep = self
            .history_tail_turns
            .min(turns.len().saturating_sub(1))
            .max(1);
        let split = turns.len() - keep;
        let older = &turns[..split];
        let kept_events = turns[split..]
            .iter()
            .flat_map(|turn| turn.iter().cloned())
            .collect::<Vec<_>>();
        let compacted = summarize_turns(older);
        let summary = merge_summaries(
            checkpoint.and_then(|checkpoint| checkpoint.summary.as_deref()),
            &compacted,
        );
        let last_compacted_seq = older
            .last()
            .and_then(|turn| turn.last())
            .map_or(0, |event| event.seq);
        let plan = checkpoint.and_then(|checkpoint| checkpoint.plan.clone());
        let tasks = checkpoint.map_or_else(Vec::new, |checkpoint| checkpoint.tasks.clone());
        self.persist_checkpoint_compacted(
            &session.session_id,
            "active",
            plan,
            tasks,
            summary.clone(),
            last_compacted_seq,
        )
        .await?;
        let new_checkpoint = read_checkpoint(&self.checkpoint_path(&session.session_id))?;
        let session_context = format_session_context(new_checkpoint.as_ref());
        let history = format_history(&kept_events);
        let bundle = self
            .context
            .build_with_history(
                Path::new(&session.workspace),
                input,
                Some(&history),
                Some(&session_context),
            )
            .await?;
        self.append_event(
            &session.session_id,
            EventKind::ContextRebuilt,
            json!({
                "turn_id": turn_id,
                "older_turns": older.len(),
                "kept_turns": keep,
                "last_compacted_seq": last_compacted_seq,
                "summary_chars": summary.as_deref().map_or(0, str::len)
            }),
        )
        .await?;
        Ok(bundle)
    }

    async fn persist_checkpoint_compacted(
        &self,
        session_id: &str,
        state: &str,
        plan: Option<String>,
        tasks: Vec<String>,
        summary: Option<String>,
        last_compacted_seq: u64,
    ) -> Result<(), RuntimeError> {
        let session = self.storage.get_session(session_id.to_owned()).await?;
        let checkpoint = CheckpointState {
            session_id: session.session_id,
            name: session.name,
            workspace: session.workspace,
            mode: session.mode,
            model_policy: session.model_policy,
            state: state.to_owned(),
            plan,
            tasks,
            summary,
            last_compacted_seq,
            updated_at: self.clock.now_rfc3339(),
        };
        write_checkpoint(&self.checkpoint_path(session_id), &checkpoint)?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_turn_inner(
        &self,
        session_id: &str,
        turn_id: &str,
        input: &str,
        model_override: Option<&str>,
        cancel: &CancellationToken,
        tools: Arc<ToolRegistry>,
        gate: PermissionGate,
        snapshots: Arc<SnapshotManager>,
    ) -> Result<(), RuntimeError> {
        let session = self.storage.get_session(session_id.to_owned()).await?;
        let role = if session.mode == SessionMode::Plan {
            "plan"
        } else {
            "build"
        };
        let mut candidates = self
            .model_candidates(&session, role, model_override)
            .await?;
        let mut model = candidates.remove(0);
        let mut pricing = self
            .providers
            .models()
            .await?
            .into_iter()
            .find(|candidate| candidate.id == model);
        let mut provider = self.providers.provider_for(&model).ok_or_else(|| {
            RuntimeError::Provider(thais_providers::ProviderError::Message(format!(
                "no provider for model {model}"
            )))
        })?;
        let bundle = self.build_prompt(&session, input, turn_id).await?;
        if let Some(info) = &pricing
            && bundle.estimated_tokens as u64 > info.context_window
        {
            self.append_event(
                session_id,
                EventKind::BudgetWarning,
                json!({
                    "turn_id": turn_id,
                    "budget": "context",
                    "used": bundle.estimated_tokens,
                    "limit": info.context_window
                }),
            )
            .await?;
            return Err(RuntimeError::InvalidRequest(
                "context window exceeded".to_owned(),
            ));
        }
        self.append_event(
            session_id,
            EventKind::TurnStarted,
            json!({"turn_id": turn_id, "input": input}),
        )
        .await?;

        let tool_definitions = tools
            .definitions()
            .into_iter()
            .map(|definition| ToolDefinition {
                name: definition.name,
                description: definition.description,
                parameters: definition.parameters,
            })
            .collect::<Vec<_>>();
        let mut messages = vec![
            ChatMessage::system(bundle.system),
            ChatMessage::user(bundle.user),
        ];
        let mut total_tokens = bundle.estimated_tokens as u64;
        let mut total_usage = Usage::default();
        let mut cost_warned = false;
        let mut repair_attempts = 0_u32;
        let started = tokio::time::Instant::now();

        for _ in 0..MAX_TOOL_ITERATIONS {
            if started.elapsed() > MAX_TURN_TIME {
                return Err(RuntimeError::InvalidRequest(
                    "time budget exhausted".to_owned(),
                ));
            }
            if total_tokens > MAX_TURN_TOKENS {
                return Err(RuntimeError::InvalidRequest(
                    "token budget exhausted".to_owned(),
                ));
            }

            let request = ChatRequest {
                model: model.clone(),
                messages: messages.clone(),
                tools: tool_definitions.clone(),
                ..ChatRequest::default()
            };
            let mut stream = match provider.chat(request).await {
                Ok(stream) => stream,
                Err(error) => {
                    let fallback = candidates
                        .iter()
                        .position(|candidate| self.providers.provider_for(candidate).is_some());
                    let Some(index) = fallback else {
                        return Err(RuntimeError::Provider(error));
                    };
                    let fallback_model = candidates.remove(index);
                    let fallback_provider = self
                        .providers
                        .provider_for(&fallback_model)
                        .expect("candidate was checked");
                    self.append_event(
                        session_id,
                        EventKind::ModelChanged,
                        json!({
                            "role": role,
                            "model": fallback_model,
                            "reason": "provider_error"
                        }),
                    )
                    .await?;
                    model = fallback_model;
                    provider = fallback_provider;
                    pricing = self
                        .providers
                        .models()
                        .await?
                        .into_iter()
                        .find(|candidate| candidate.id == model);
                    continue;
                }
            };
            let mut content = String::new();
            let mut accumulators: HashMap<usize, ToolAccumulator> = HashMap::new();
            let mut usage = Usage::default();

            loop {
                tokio::select! {
                    () = cancel.cancelled() => {
                        self.append_event(
                            session_id,
                            EventKind::TurnCancelled,
                            json!({"turn_id": turn_id}),
                        )
                        .await?;
                        return Ok(());
                    }
                    item = stream.next() => {
                        match item {
                            None => break,
                            Some(Ok(ChatEvent::Content { text })) => {
                                total_tokens = total_tokens
                                    .saturating_add(estimate_tokens(&text) as u64);
                                content.push_str(&text);
                                self.append_event(
                                    session_id,
                                    EventKind::ModelDelta,
                                    json!({"turn_id": turn_id, "text": text}),
                                )
                                .await?;
                            }
                            Some(Ok(ChatEvent::Reasoning { text })) => {
                                total_tokens = total_tokens
                                    .saturating_add(estimate_tokens(&text) as u64);
                                self.append_event(
                                    session_id,
                                    EventKind::ReasoningDelta,
                                    json!({"turn_id": turn_id, "text": text}),
                                )
                                .await?;
                            }
                            Some(Ok(ChatEvent::ToolCallDelta {
                                index,
                                id,
                                name,
                                arguments,
                            })) => {
                                let accumulator = accumulators.entry(index).or_default();
                                if let Some(id) = id {
                                    accumulator.id = Some(id);
                                }
                                if let Some(name) = name {
                                    accumulator.name = Some(name);
                                }
                                accumulator.arguments.push_str(&arguments);
                            }
                            Some(Ok(ChatEvent::Usage { usage: reported })) => {
                                usage = reported;
                                total_tokens = total_tokens.saturating_add(
                                    reported.input_tokens + reported.output_tokens,
                                );
                            }
                            Some(Ok(ChatEvent::Done)) => break,
                            Some(Err(error)) => return Err(RuntimeError::Provider(error)),
                        }
                    }
                }
            }

            total_usage.input_tokens = total_usage.input_tokens.saturating_add(usage.input_tokens);
            total_usage.output_tokens = total_usage
                .output_tokens
                .saturating_add(usage.output_tokens);

            let cost_so_far = pricing.as_ref().map_or(0.0, |pricing| {
                (total_usage.input_tokens as f64 * pricing.input_cost_per_million
                    + total_usage.output_tokens as f64 * pricing.output_cost_per_million)
                    / 1_000_000.0
            });
            if cost_so_far > self.max_turn_cost_usd {
                self.append_event(
                    session_id,
                    EventKind::BudgetWarning,
                    json!({
                        "turn_id": turn_id,
                        "budget": "cost",
                        "used": cost_so_far,
                        "limit": self.max_turn_cost_usd
                    }),
                )
                .await?;
                return Err(RuntimeError::InvalidRequest(
                    "cost budget exhausted".to_owned(),
                ));
            }
            if !cost_warned && cost_so_far >= self.max_turn_cost_usd * 0.8 {
                cost_warned = true;
                self.append_event(
                    session_id,
                    EventKind::BudgetWarning,
                    json!({
                        "turn_id": turn_id,
                        "budget": "cost",
                        "used": cost_so_far,
                        "limit": self.max_turn_cost_usd
                    }),
                )
                .await?;
            }

            if accumulators.is_empty() {
                if session.mode == SessionMode::Plan && !content.trim().is_empty() {
                    let decision = self
                        .request_approval(session_id, json!({"type": "plan", "plan": content}))
                        .await?;
                    if decision == ApprovalDecision::Allow {
                        self.storage
                            .update_session_mode(session_id.to_owned(), SessionMode::Build)
                            .await?;
                        let tools = ToolRegistry::read_write(Path::new(&session.workspace))?;
                        let mut sessions = self.sessions.lock().await;
                        if let Some(state) = sessions.get_mut(session_id) {
                            state.tools = Arc::new(tools);
                            state.gate = PermissionGate::new(PolicyMode::Build);
                        }
                    }
                }
                let plan = if session.mode == SessionMode::Plan {
                    Some(content)
                } else {
                    None
                };
                let tasks = plan.as_deref().map_or_else(Vec::new, derive_tasks);
                self.persist_checkpoint(session_id, "ready", plan, tasks)
                    .await?;
                let cost_usd = pricing.as_ref().map_or(0.0, |pricing| {
                    (total_usage.input_tokens as f64 * pricing.input_cost_per_million
                        + total_usage.output_tokens as f64 * pricing.output_cost_per_million)
                        / 1_000_000.0
                });
                let updated = self
                    .storage
                    .record_session_usage(
                        session_id.to_owned(),
                        total_usage.input_tokens,
                        total_usage.output_tokens,
                        cost_usd,
                    )
                    .await?;
                self.append_event(
                    session_id,
                    EventKind::TurnCompleted,
                    json!({
                        "turn_id": turn_id,
                        "model": model,
                        "usage": {
                            "input_tokens": total_usage.input_tokens,
                            "output_tokens": total_usage.output_tokens
                        },
                        "session_usage": {
                            "input_tokens": updated.input_tokens,
                            "output_tokens": updated.output_tokens,
                            "cost_usd": updated.cost_usd
                        },
                        "cost_usd": cost_usd
                    }),
                )
                .await?;
                return Ok(());
            }

            let mut accumulators = accumulators.into_iter().collect::<Vec<_>>();
            accumulators.sort_by_key(|(index, _)| *index);
            let mut tool_calls = Vec::new();
            for (index, accumulator) in accumulators {
                let name = accumulator.name.ok_or_else(|| {
                    RuntimeError::InvalidRequest(format!("tool call {index} has no name"))
                })?;
                let arguments = if accumulator.arguments.trim().is_empty() {
                    "{}".to_owned()
                } else {
                    accumulator.arguments
                };
                tool_calls.push(ToolCall {
                    id: accumulator.id.unwrap_or_else(|| self.ids.new_id("call")),
                    name,
                    arguments,
                });
            }

            for call in &tool_calls {
                self.append_event(
                    session_id,
                    EventKind::ToolCall,
                    json!({
                        "turn_id": turn_id,
                        "id": call.id,
                        "name": call.name,
                        "arguments": call.arguments
                    }),
                )
                .await?;
            }

            let assistant_content = if content.trim().is_empty() {
                None
            } else {
                Some(content)
            };
            messages.push(ChatMessage::assistant(
                assistant_content,
                tool_calls.clone(),
            ));

            let mut prepared = Vec::new();
            for call in tool_calls {
                if cancel.is_cancelled() {
                    self.append_event(
                        session_id,
                        EventKind::TurnCancelled,
                        json!({"turn_id": turn_id}),
                    )
                    .await?;
                    return Ok(());
                }

                match serde_json::from_str::<Value>(&call.arguments) {
                    Err(error) => prepared.push(PreparedTool::Result {
                        call,
                        result: ToolResult {
                            content: format!("invalid JSON arguments: {error}"),
                            is_error: true,
                            diff: None,
                        },
                    }),
                    Ok(arguments) => match gate.check(
                        &call.name,
                        &arguments,
                        tools.jail(),
                        tools.is_known(&call.name),
                        tools.is_mutating(&call.name),
                        &tools.summary(&call.name, &arguments),
                    ) {
                        PermissionDecision::Allow => {
                            self.append_event(
                                session_id,
                                EventKind::PermissionGranted,
                                json!({
                                    "turn_id": turn_id,
                                    "tool": call.name.clone(),
                                    "summary": tools.summary(&call.name, &arguments)
                                }),
                            )
                            .await?;
                            prepared.push(PreparedTool::Execute { call, arguments });
                        }
                        PermissionDecision::Ask(reason) => {
                            let decision = self
                                .request_approval(
                                    session_id,
                                    json!({
                                        "tool": call.name.clone(),
                                        "arguments": arguments.clone(),
                                        "reason": reason,
                                        "diff": tools.preview(&call.name, &arguments)
                                    }),
                                )
                                .await?;
                            if decision == ApprovalDecision::Allow {
                                prepared.push(PreparedTool::Execute { call, arguments });
                            } else {
                                prepared.push(PreparedTool::Result {
                                    call,
                                    result: ToolResult {
                                        content: "denied by user".to_owned(),
                                        is_error: true,
                                        diff: None,
                                    },
                                });
                            }
                        }
                        PermissionDecision::Deny(reason) => {
                            self.append_event(
                                session_id,
                                EventKind::PermissionDenied,
                                json!({
                                    "turn_id": turn_id,
                                    "tool": call.name.clone(),
                                    "reason": reason.clone()
                                }),
                            )
                            .await?;
                            prepared.push(PreparedTool::Result {
                                call,
                                result: ToolResult {
                                    content: format!("denied: {reason}"),
                                    is_error: true,
                                    diff: None,
                                },
                            });
                        }
                    },
                }
            }

            let mut executed = HashMap::new();
            let mut tasks = Vec::new();
            let mut mutating_calls = Vec::new();
            for (index, item) in prepared.iter().enumerate() {
                if let PreparedTool::Execute { call, arguments } = item {
                    if call.name == "subagent" {
                        let kind = arguments
                            .get("kind")
                            .and_then(Value::as_str)
                            .unwrap_or("explore");
                        let task = arguments.get("task").and_then(Value::as_str).unwrap_or("");
                        let result = self
                            .run_subagent(&session, kind, task, cancel, turn_id)
                            .await;
                        executed.insert(index, result);
                        continue;
                    }
                    if tools.is_mutating(&call.name) {
                        mutating_calls.push((index, call.clone(), arguments.clone()));
                    } else {
                        let tools = Arc::clone(&tools);
                        let name = call.name.clone();
                        let args = arguments.clone();
                        tasks.push((
                            index,
                            tokio::task::spawn_blocking(move || tools.execute(&name, args)),
                        ));
                    }
                }
            }

            for (index, task) in tasks {
                let result = match task.await {
                    Ok(Ok(result)) => Ok(redact_tool_result(result)),
                    Ok(Err(error)) => Err(error),
                    Err(error) => Err(ToolError::InvalidArgs(format!("tool task failed: {error}"))),
                };
                executed.insert(index, result);
            }

            for (index, call, arguments) in mutating_calls {
                if call.name == "memory_write" {
                    let result = tools.execute(&call.name, arguments.clone());
                    if let Ok(result) = &result {
                        let rule = arguments.get("rule").and_then(Value::as_str).unwrap_or("");
                        self.append_event(
                            session_id,
                            EventKind::MemoryUpdated,
                            json!({
                                "turn_id": turn_id,
                                "rule": rule,
                                "diff": result.diff
                            }),
                        )
                        .await?;
                    }
                    executed.insert(index, result);
                    continue;
                }
                let snapshot = match snapshots.create() {
                    Ok(snapshot) => {
                        self.append_event(
                            session_id,
                            EventKind::SnapshotCreated,
                            json!({"turn_id": turn_id, "commit": snapshot.commit}),
                        )
                        .await?;
                        snapshot
                    }
                    Err(error) => {
                        executed.insert(
                            index,
                            Err(ToolError::Command(format!("snapshot failed: {error}"))),
                        );
                        continue;
                    }
                };

                let result = tools
                    .execute(&call.name, arguments.clone())
                    .map(redact_tool_result);
                if cancel.is_cancelled() {
                    let restored = snapshots.restore(&snapshot).unwrap_or_default();
                    self.append_event(
                        session_id,
                        EventKind::RollbackCompleted,
                        json!({"turn_id": turn_id, "paths": restored}),
                    )
                    .await?;
                    self.append_event(
                        session_id,
                        EventKind::TurnCancelled,
                        json!({"turn_id": turn_id}),
                    )
                    .await?;
                    return Ok(());
                }
                match result {
                    Ok(mut result) => {
                        if let Some(path) = arguments.get("path").and_then(Value::as_str)
                            && let Ok(path) = tools.jail().resolve(path)
                            && let Some(report) = tools.lsp_report(&path)
                        {
                            self.append_event(
                                session_id,
                                EventKind::LspDiagnostics,
                                json!({
                                    "turn_id": turn_id,
                                    "path": tools.jail().display(&path),
                                    "diagnostics": report.text,
                                    "has_errors": report.has_errors
                                }),
                            )
                            .await?;
                            if report.has_errors {
                                let restored = snapshots.restore(&snapshot).unwrap_or_default();
                                self.append_event(
                                    session_id,
                                    EventKind::VerificationFailed,
                                    json!({
                                        "turn_id": turn_id,
                                        "output": format!("LSP diagnostics:\n{}", report.text)
                                    }),
                                )
                                .await?;
                                self.append_event(
                                    session_id,
                                    EventKind::RollbackCompleted,
                                    json!({"turn_id": turn_id, "paths": restored}),
                                )
                                .await?;
                                repair_attempts += 1;
                                executed.insert(
                                    index,
                                    Ok(ToolResult {
                                        content: format!(
                                            "LSP diagnostics failed; changes rolled back.\n{}",
                                            report.text
                                        ),
                                        is_error: true,
                                        diff: None,
                                    }),
                                );
                                if repair_attempts >= 2 {
                                    return Err(RuntimeError::InvalidRequest(
                                        "repair budget exhausted".to_owned(),
                                    ));
                                }
                                continue;
                            }
                        }
                        if let Some(verifier) = &self.verifier {
                            self.append_event(
                                session_id,
                                EventKind::VerificationStarted,
                                json!({"turn_id": turn_id, "commands": verifier.commands()}),
                            )
                            .await?;
                            match verifier.run(tools.jail().root()) {
                                Verification::Pass => {
                                    self.append_event(
                                        session_id,
                                        EventKind::VerificationPassed,
                                        json!({"turn_id": turn_id}),
                                    )
                                    .await?;
                                    executed.insert(index, Ok(result));
                                }
                                Verification::Fail { output } => {
                                    let output = truncate_output(&output);
                                    self.append_event(
                                        session_id,
                                        EventKind::VerificationFailed,
                                        json!({"turn_id": turn_id, "output": output}),
                                    )
                                    .await?;
                                    let restored = snapshots.restore(&snapshot).unwrap_or_default();
                                    self.append_event(
                                        session_id,
                                        EventKind::RollbackCompleted,
                                        json!({"turn_id": turn_id, "paths": restored}),
                                    )
                                    .await?;
                                    repair_attempts += 1;
                                    executed.insert(
                                        index,
                                        Ok(ToolResult {
                                            content: format!(
                                                "verification failed; changes rolled back. output:\n{output}"
                                            ),
                                            is_error: true,
                                            diff: None,
                                        }),
                                    );
                                    if repair_attempts >= 2 {
                                        return Err(RuntimeError::InvalidRequest(
                                            "repair budget exhausted".to_owned(),
                                        ));
                                    }
                                }
                                Verification::Unavailable { reason } => {
                                    self.append_event(
                                        session_id,
                                        EventKind::VerificationUnavailable,
                                        json!({"turn_id": turn_id, "reason": reason}),
                                    )
                                    .await?;
                                    result.content =
                                        format!("{}\n[unverified: {reason}]", result.content);
                                    executed.insert(index, Ok(result));
                                }
                            }
                        } else {
                            executed.insert(index, Ok(result));
                        }
                    }
                    Err(error) => {
                        let restored = snapshots.restore(&snapshot).unwrap_or_default();
                        self.append_event(
                            session_id,
                            EventKind::RollbackCompleted,
                            json!({"turn_id": turn_id, "paths": restored}),
                        )
                        .await?;
                        executed.insert(index, Err(error));
                    }
                }
            }

            if cancel.is_cancelled() {
                self.append_event(
                    session_id,
                    EventKind::TurnCancelled,
                    json!({"turn_id": turn_id}),
                )
                .await?;
                return Ok(());
            }

            for (index, item) in prepared.into_iter().enumerate() {
                let (call, result) = match item {
                    PreparedTool::Result { call, result } => (call, result),
                    PreparedTool::Execute { call, .. } => {
                        let result = executed.remove(&index).unwrap_or_else(|| {
                            Err(ToolError::InvalidArgs("missing tool result".to_owned()))
                        });
                        let result = match result {
                            Ok(result) => result,
                            Err(error) => ToolResult {
                                content: error.to_string(),
                                is_error: true,
                                diff: None,
                            },
                        };
                        (call, result)
                    }
                };

                total_tokens = total_tokens
                    .saturating_add(estimate_tokens(&call.arguments) as u64)
                    .saturating_add(estimate_tokens(&result.content) as u64);
                self.append_event(
                    session_id,
                    EventKind::ToolResult,
                    json!({
                        "turn_id": turn_id,
                        "tool_call_id": call.id,
                        "name": call.name,
                        "content": result.content,
                        "is_error": result.is_error,
                        "diff": result.diff
                    }),
                )
                .await?;

                let tool_content = if result.is_error {
                    format!("[error] {}", result.content)
                } else {
                    result.content
                };
                messages.push(ChatMessage::tool(call.id, tool_content));
            }
        }

        Err(RuntimeError::InvalidRequest(
            "tool iteration budget exhausted".to_owned(),
        ))
    }
}

#[derive(Default)]
struct ToolAccumulator {
    id: Option<String>,
    name: Option<String>,
    arguments: String,
}

enum PreparedTool {
    Execute { call: ToolCall, arguments: Value },
    Result { call: ToolCall, result: ToolResult },
}

fn redact_tool_result(mut result: ToolResult) -> ToolResult {
    let redactor = BasicRedactor;
    redactor.redact_text(&mut result.content);
    if let Some(diff) = result.diff.as_mut() {
        redactor.redact_text(diff);
    }
    result
}

fn derive_tasks(plan: &str) -> Vec<String> {
    plan.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter_map(|line| {
            let task = line
                .strip_prefix("- [ ] ")
                .or_else(|| line.strip_prefix("- [x] "))
                .or_else(|| line.strip_prefix("- "))
                .or_else(|| line.strip_prefix("* "))
                .or_else(|| {
                    let mut parts = line.splitn(2, '.');
                    let number = parts.next()?;
                    if number.chars().all(|character| character.is_ascii_digit())
                        && let Some(rest) = parts.next()
                    {
                        Some(rest.trim())
                    } else {
                        None
                    }
                })?;
            (!task.is_empty()).then(|| truncate_text(task, 200))
        })
        .take(20)
        .collect()
}

fn split_turns(events: &[EventEnvelope]) -> Vec<Vec<EventEnvelope>> {
    let mut turns = Vec::new();
    let mut current: Option<Vec<EventEnvelope>> = None;
    for event in events {
        if event.kind == EventKind::TurnStarted {
            if let Some(turn) = current.take()
                && !turn.is_empty()
            {
                turns.push(turn);
            }
            current = Some(vec![event.clone()]);
        } else if let Some(turn) = current.as_mut() {
            turn.push(event.clone());
        }
    }
    if let Some(turn) = current
        && !turn.is_empty()
    {
        turns.push(turn);
    }
    turns
}

fn tail_turns(events: &[EventEnvelope], keep: usize) -> Vec<EventEnvelope> {
    let turns = split_turns(events);
    let start = turns.len().saturating_sub(keep.max(1));
    turns[start..]
        .iter()
        .flat_map(|turn| turn.iter().cloned())
        .collect()
}

fn format_history(events: &[EventEnvelope]) -> String {
    let mut output = String::new();
    for turn in split_turns(events) {
        let mut assistant = String::new();
        for event in turn {
            match event.kind {
                EventKind::TurnStarted => {
                    flush_assistant(&mut assistant, &mut output);
                    if let Some(input) = event.payload.get("input").and_then(Value::as_str) {
                        output.push_str("\nUser: ");
                        output.push_str(&truncate_output(input));
                    }
                }
                EventKind::ModelDelta => {
                    if let Some(text) = event.payload.get("text").and_then(Value::as_str) {
                        assistant.push_str(text);
                    }
                }
                EventKind::ToolCall => {
                    flush_assistant(&mut assistant, &mut output);
                    let name = event
                        .payload
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("tool");
                    let arguments = event
                        .payload
                        .get("arguments")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    output.push_str(&format!("\nTool: {name} {}", truncate_text(arguments, 400)));
                }
                EventKind::ToolResult => {
                    flush_assistant(&mut assistant, &mut output);
                    let name = event
                        .payload
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("tool");
                    let content = event
                        .payload
                        .get("content")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    output.push_str(&format!(
                        "\nResult({name}): {}",
                        truncate_text(content, 600)
                    ));
                }
                EventKind::PermissionResolved => {
                    flush_assistant(&mut assistant, &mut output);
                    let decision = event
                        .payload
                        .get("decision")
                        .and_then(Value::as_str)
                        .unwrap_or("resolved");
                    output.push_str(&format!("\nApproval: {decision}"));
                }
                EventKind::Error => {
                    flush_assistant(&mut assistant, &mut output);
                    let message = event
                        .payload
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("error");
                    output.push_str(&format!("\nError: {}", truncate_text(message, 400)));
                }
                _ => {}
            }
        }
        flush_assistant(&mut assistant, &mut output);
    }
    output
}

fn flush_assistant(assistant: &mut String, output: &mut String) {
    if !assistant.trim().is_empty() {
        output.push_str("\nAssistant: ");
        output.push_str(&truncate_text(assistant, 2_000));
    }
    assistant.clear();
}

fn format_session_context(checkpoint: Option<&CheckpointState>) -> String {
    let Some(checkpoint) = checkpoint else {
        return "(none)".to_owned();
    };
    let mut output = format!("State: {}\n", checkpoint.state);
    if let Some(plan) = &checkpoint.plan {
        output.push_str("Approved plan:\n");
        output.push_str(&truncate_text(plan, 2_000));
        output.push('\n');
    }
    if !checkpoint.tasks.is_empty() {
        output.push_str("Tasks:\n");
        for task in &checkpoint.tasks {
            output.push_str("- [ ] ");
            output.push_str(task);
            output.push('\n');
        }
    }
    if let Some(summary) = &checkpoint.summary {
        output.push_str("Compacted history:\n");
        output.push_str(summary);
        output.push('\n');
    }
    output
}

fn summarize_turns(turns: &[Vec<EventEnvelope>]) -> String {
    let mut requests = Vec::new();
    let mut tools: BTreeMap<String, usize> = BTreeMap::new();
    let mut decisions = Vec::new();
    for turn in turns {
        for event in turn {
            match event.kind {
                EventKind::TurnStarted => {
                    if let Some(input) = event.payload.get("input").and_then(Value::as_str) {
                        requests.push(truncate_text(input, 200));
                    }
                }
                EventKind::ToolCall => {
                    let name = event
                        .payload
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("tool");
                    *tools.entry(name.to_owned()).or_insert(0) += 1;
                }
                EventKind::PermissionResolved => {
                    if let Some(decision) = event.payload.get("decision").and_then(Value::as_str) {
                        decisions.push(decision.to_owned());
                    }
                }
                _ => {}
            }
        }
    }
    let mut output = format!("Compacted {} earlier turns.\n", turns.len());
    if !requests.is_empty() {
        output.push_str("Requests:\n");
        for request in requests.iter().take(8) {
            output.push_str("- ");
            output.push_str(request);
            output.push('\n');
        }
    }
    if !tools.is_empty() {
        output.push_str("Tools: ");
        let rendered = tools
            .iter()
            .map(|(name, count)| format!("{name} x{count}"))
            .collect::<Vec<_>>()
            .join(", ");
        output.push_str(&truncate_text(&rendered, 500));
        output.push('\n');
    }
    if !decisions.is_empty() {
        output.push_str(&format!("Approvals: {}\n", decisions.join(", ")));
    }
    truncate_text(&output, 4_000)
}

fn merge_summaries(previous: Option<&str>, new: &str) -> Option<String> {
    let new = new.trim();
    match (
        previous.map(str::trim).filter(|value| !value.is_empty()),
        new.is_empty(),
    ) {
        (Some(previous), false) => Some(truncate_text(&format!("{previous}\n\n{new}"), 4_000)),
        (Some(previous), true) => Some(previous.to_owned()),
        (None, false) => Some(new.to_owned()),
        (None, true) => None,
    }
}

fn truncate_text(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        text.to_owned()
    } else {
        format!("{}…", text.chars().take(max_chars).collect::<String>())
    }
}

fn truncate_output(text: &str) -> String {
    const MAX: usize = 4_000;
    if text.chars().count() <= MAX {
        text.to_owned()
    } else {
        format!("{}…", text.chars().take(MAX).collect::<String>())
    }
}
