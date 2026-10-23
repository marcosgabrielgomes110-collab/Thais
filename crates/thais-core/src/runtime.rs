//! Session runtime: create sessions, run turns, stream events, and resolve
//! approvals and cancellations.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

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
use thais_providers::{ChatEvent, ChatMessage, ChatRequest, Provider, Role, Usage};
use thais_storage::{NewEvent, StorageError, StorageHandle};
use thiserror::Error;
use tokio::sync::{Mutex, broadcast, oneshot};
use tokio_util::sync::CancellationToken;

use crate::clock::Clock;
use crate::ids::IdGenerator;

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
}

impl RuntimeError {
    fn to_error_object(&self) -> ErrorObject {
        let code = match self {
            Self::SessionNotFound(_)
            | Self::TurnInProgress(_)
            | Self::ApprovalNotFound(_)
            | Self::InvalidRequest(_)
            | Self::InvalidWorkspace => INVALID_PARAMS,
            Self::Storage(_) | Self::Provider(_) => INTERNAL_ERROR,
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
    ) -> Self {
        let (events, _) = broadcast::channel(1024);
        Self {
            storage,
            provider,
            clock,
            ids,
            sessions: Mutex::new(HashMap::new()),
            approvals: Mutex::new(HashMap::new()),
            events,
        }
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
        self.sessions
            .lock()
            .await
            .insert(session_id.clone(), SessionState { turn: None });
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

        let runtime = Arc::clone(self);
        let session = session_id.to_owned();
        let turn = turn_id.clone();
        tokio::spawn(async move {
            runtime.run_turn(session, turn, input, cancel).await;
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
            json!({"approval_id": approval_id, "action": action}),
        )
        .await?;
        receiver
            .await
            .map_err(|_| RuntimeError::ApprovalNotFound(approval_id))
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
        let mut live = self.subscribe_live();
        let session = self
            .create_session(workspace, SessionMode::Build, model)
            .await?;
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

    async fn run_turn(
        self: Arc<Self>,
        session_id: String,
        turn_id: String,
        input: String,
        cancel: CancellationToken,
    ) {
        if let Err(error) = self
            .run_turn_inner(&session_id, &turn_id, &input, &cancel)
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

    async fn run_turn_inner(
        &self,
        session_id: &str,
        turn_id: &str,
        input: &str,
        cancel: &CancellationToken,
    ) -> Result<(), RuntimeError> {
        let session = self.storage.get_session(session_id.to_owned()).await?;
        self.append_event(
            session_id,
            EventKind::TurnStarted,
            json!({"turn_id": turn_id, "input": input}),
        )
        .await?;

        let request = ChatRequest {
            model: session.model,
            messages: vec![ChatMessage {
                role: Role::User,
                content: input.to_owned(),
            }],
            ..ChatRequest::default()
        };
        let mut stream = self.provider.chat(request).await?;
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
                            self.append_event(
                                session_id,
                                EventKind::ModelDelta,
                                json!({"turn_id": turn_id, "text": text}),
                            )
                            .await?;
                        }
                        Some(Ok(ChatEvent::Reasoning { text })) => {
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
                            self.append_event(
                                session_id,
                                EventKind::ToolCall,
                                json!({
                                    "turn_id": turn_id,
                                    "index": index,
                                    "id": id,
                                    "name": name,
                                    "arguments": arguments
                                }),
                            )
                            .await?;
                        }
                        Some(Ok(ChatEvent::Usage { usage: reported })) => {
                            usage = reported;
                        }
                        Some(Ok(ChatEvent::Done)) => break,
                        Some(Err(error)) => return Err(RuntimeError::Provider(error)),
                    }
                }
            }
        }

        self.append_event(
            session_id,
            EventKind::TurnCompleted,
            json!({
                "turn_id": turn_id,
                "usage": {
                    "input_tokens": usage.input_tokens,
                    "output_tokens": usage.output_tokens
                }
            }),
        )
        .await?;
        Ok(())
    }
}
