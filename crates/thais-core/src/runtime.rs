//! Session runtime: create sessions, run turns, stream events, and resolve
//! approvals and cancellations.

use std::collections::HashMap;
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
    ApprovalDecision, InitializeParams, InitializeResult, SessionApproveParams,
    SessionApproveResult, SessionCancelParams, SessionCancelResult, SessionCreateParams,
    SessionCreateResult, SessionInfo, SessionMode, SessionSendParams, SessionSendResult,
    SessionStatus, SessionSubscribeParams, SessionSubscribeResult,
};
use thais_providers::{
    ChatEvent, ChatMessage, ChatRequest, Provider, ToolCall, ToolDefinition, Usage,
};
use thais_storage::{NewEvent, StorageError, StorageHandle};
use thais_tools::{
    PermissionDecision, PermissionGate, PolicyMode, ToolError, ToolRegistry, ToolResult,
};
use thiserror::Error;
use tokio::sync::{Mutex, broadcast, oneshot};
use tokio_util::sync::CancellationToken;

use crate::clock::Clock;
use crate::context::{ContextError, ContextManager, estimate_tokens};
use crate::ids::IdGenerator;
use crate::replay::{ReplayError, ReplayState};
use crate::snapshot::{SnapshotError, SnapshotManager};
use crate::verifier::{Verification, Verifier};

const MAX_TOOL_ITERATIONS: usize = 8;
const MAX_TURN_TOKENS: u64 = 50_000;
const MAX_TURN_TIME: Duration = Duration::from_secs(180);
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(60);

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
}

impl RuntimeError {
    fn to_error_object(&self) -> ErrorObject {
        let code = match self {
            Self::SessionNotFound(_)
            | Self::TurnInProgress(_)
            | Self::ApprovalNotFound(_)
            | Self::InvalidRequest(_)
            | Self::InvalidWorkspace => INVALID_PARAMS,
            Self::Storage(_)
            | Self::Provider(_)
            | Self::Context(_)
            | Self::Tool(_)
            | Self::Replay(_)
            | Self::Snapshot(_) => INTERNAL_ERROR,
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
    provider: Arc<dyn Provider>,
    clock: Arc<dyn Clock>,
    ids: Arc<dyn IdGenerator>,
    context: ContextManager,
    snapshot_base: Option<PathBuf>,
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
            provider,
            clock,
            ids,
            context,
            snapshot_base: None,
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
                .provider
                .models()
                .await?
                .first()
                .map(|model| model.id.clone())
                .unwrap_or_else(|| "fake-model".to_owned()),
        };
        let session_id = self.ids.new_id("session");
        let timestamp = self.clock.now_rfc3339();
        let info = SessionInfo {
            session_id: session_id.clone(),
            workspace: canonical,
            mode,
            model,
            status: SessionStatus::Active,
            created_at: timestamp.clone(),
            updated_at: timestamp,
        };

        self.storage.create_session(info.clone()).await?;
        let tools = match info.mode {
            SessionMode::Plan => ToolRegistry::read_only(Path::new(&info.workspace))?,
            SessionMode::Build => ToolRegistry::read_write(Path::new(&info.workspace))?,
        };
        let gate = PermissionGate::new(match info.mode {
            SessionMode::Plan => PolicyMode::Plan,
            SessionMode::Build => PolicyMode::Build,
        });
        let snapshot_root = self
            .snapshot_base
            .clone()
            .unwrap_or_else(|| std::env::temp_dir().join("thais-snapshots"));
        let snapshots =
            SnapshotManager::new(Path::new(&info.workspace), &snapshot_root.join(&session_id))?;
        self.sessions.lock().await.insert(
            session_id.clone(),
            SessionState {
                turn: None,
                tools: Arc::new(tools),
                gate,
                snapshots: Arc::new(snapshots),
            },
        );
        self.append_event(
            &session_id,
            EventKind::SessionCreated,
            json!({"session": &info}),
        )
        .await?;
        Ok(info)
    }

    /// Fetch session metadata.
    pub async fn get_session(&self, session_id: &str) -> Result<SessionInfo, RuntimeError> {
        Ok(self.storage.get_session(session_id.to_owned()).await?)
    }

    /// List session metadata.
    pub async fn list_sessions(&self) -> Result<Vec<SessionInfo>, RuntimeError> {
        Ok(self.storage.list_sessions().await?)
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
                .run_turn(session, turn, input, cancel, tools, gate, snapshots)
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
        let Some(sender) = sender else {
            return Ok(false);
        };
        let _ = sender.send(decision);
        self.append_event(
            session_id,
            EventKind::PermissionResolved,
            json!({"approval_id": approval_id, "decision": decision}),
        )
        .await?;
        Ok(true)
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
        self.append_event(
            session_id,
            EventKind::PermissionRequested,
            json!({"approval_id": approval_id.clone(), "action": action}),
        )
        .await?;

        match tokio::time::timeout(APPROVAL_TIMEOUT, receiver).await {
            Ok(Ok(decision)) => Ok(decision),
            Ok(Err(_)) => Err(RuntimeError::ApprovalNotFound(approval_id)),
            Err(_) => {
                self.approvals.lock().await.remove(&approval_id);
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
                    .create_session(params.workspace, params.mode, params.model)
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
                    .send(&params.session_id, params.input, params.turn_id)
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
    async fn run_turn_inner(
        &self,
        session_id: &str,
        turn_id: &str,
        input: &str,
        cancel: &CancellationToken,
        tools: Arc<ToolRegistry>,
        gate: PermissionGate,
        snapshots: Arc<SnapshotManager>,
    ) -> Result<(), RuntimeError> {
        let session = self.storage.get_session(session_id.to_owned()).await?;
        let bundle = self
            .context
            .build(Path::new(&session.workspace), input)
            .await?;
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
                model: session.model.clone(),
                messages: messages.clone(),
                tools: tool_definitions.clone(),
                ..ChatRequest::default()
            };
            let mut stream = self.provider.chat(request).await?;
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
                self.append_event(
                    session_id,
                    EventKind::TurnCompleted,
                    json!({
                        "turn_id": turn_id,
                        "usage": {
                            "input_tokens": total_usage.input_tokens,
                            "output_tokens": total_usage.output_tokens
                        }
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

            let mut tasks = Vec::new();
            let mut mutating_calls = Vec::new();
            for (index, item) in prepared.iter().enumerate() {
                if let PreparedTool::Execute { call, arguments } = item {
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

            let mut executed = HashMap::new();
            for (index, task) in tasks {
                let result = match task.await {
                    Ok(result) => result,
                    Err(error) => Err(ToolError::InvalidArgs(format!("tool task failed: {error}"))),
                };
                executed.insert(index, result);
            }

            for (index, call, arguments) in mutating_calls {
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

                let result = tools.execute(&call.name, arguments);
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

fn truncate_output(text: &str) -> String {
    const MAX: usize = 4_000;
    if text.chars().count() <= MAX {
        text.to_owned()
    } else {
        format!("{}…", text.chars().take(MAX).collect::<String>())
    }
}
