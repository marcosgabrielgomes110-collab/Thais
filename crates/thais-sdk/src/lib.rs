#![forbid(unsafe_code)]

//! Client SDK for the Thais Agent API. Clients depend on this crate and
//! `thais-protocol`; they never import `thais-core`.

pub mod channel;

use std::sync::atomic::{AtomicI64, Ordering};

use async_trait::async_trait;
use futures_util::stream::BoxStream;
use serde_json::Value;
use thais_protocol::error::ProtocolError;
use thais_protocol::jsonrpc::{Notification, Request, Response};
use thais_protocol::method;
use thais_protocol::session::{
    HistorySearchParams, HistorySearchResult, InitializeParams, InitializeResult, ModelsListResult,
    SessionApproveParams, SessionApproveResult, SessionArchiveParams, SessionArchiveResult,
    SessionCancelParams, SessionCancelResult, SessionCreateParams, SessionCreateResult,
    SessionDeleteParams, SessionDeleteResult, SessionGetParams, SessionGetResult,
    SessionListParams, SessionListResult, SessionReleaseLeaseParams, SessionReleaseLeaseResult,
    SessionRenameParams, SessionRenameResult, SessionResumeParams, SessionResumeResult,
    SessionSendParams, SessionSendResult, SessionSetModelParams, SessionSetModelResult,
    SessionSubscribeParams, SessionSubscribeResult,
};
use thiserror::Error;

pub use channel::{ClientTransport, ServerTransport, empty_notifications, pair};
pub use thais_protocol as protocol;

/// A bidirectional Agent API transport.
#[async_trait]
pub trait Transport: Send + Sync {
    /// Send one request and await its response.
    async fn send(&self, request: Request) -> Result<Response, SdkError>;

    /// Stream of server notifications.
    fn notifications(&self) -> BoxStream<'static, Notification>;
}

/// SDK failures.
#[derive(Debug, Error)]
pub enum SdkError {
    /// Transport-level failure.
    #[error("transport error: {0}")]
    Transport(String),
    /// Protocol validation or parsing failure.
    #[error("protocol error: {0}")]
    Protocol(#[from] ProtocolError),
    /// JSON serialization failure.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    /// Error returned by the server.
    #[error("remote error {code}: {message}")]
    Remote {
        /// JSON-RPC error code.
        code: i64,
        /// Error message.
        message: String,
    },
}

/// Typed client over any [`Transport`].
pub struct Client<T> {
    transport: T,
    next_id: AtomicI64,
}

impl<T: Transport> Client<T> {
    /// Build a client.
    #[must_use]
    pub fn new(transport: T) -> Self {
        Self {
            transport,
            next_id: AtomicI64::new(1),
        }
    }

    /// Server notifications.
    pub fn notifications(&self) -> BoxStream<'static, Notification> {
        self.transport.notifications()
    }

    /// Send a raw request and return the result value.
    pub async fn request(&self, method: &str, params: Value) -> Result<Value, SdkError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let request = Request::new(id, method, params);
        let response = self.transport.send(request).await?;

        if let Some(error) = response.error {
            return Err(SdkError::Remote {
                code: error.code,
                message: error.message,
            });
        }
        response.result.ok_or_else(|| {
            SdkError::Protocol(ProtocolError::InvalidRequest(
                "response has neither result nor error".to_owned(),
            ))
        })
    }

    /// Perform the protocol handshake.
    pub async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult, SdkError> {
        let value = self
            .request(method::INITIALIZE, serde_json::to_value(params)?)
            .await?;
        Ok(serde_json::from_value(value)?)
    }

    /// Create a session.
    pub async fn session_create(
        &self,
        params: SessionCreateParams,
    ) -> Result<SessionCreateResult, SdkError> {
        let value = self
            .request(method::SESSION_CREATE, serde_json::to_value(params)?)
            .await?;
        Ok(serde_json::from_value(value)?)
    }

    /// List sessions.
    pub async fn session_list(
        &self,
        params: SessionListParams,
    ) -> Result<SessionListResult, SdkError> {
        let value = self
            .request(method::SESSION_LIST, serde_json::to_value(params)?)
            .await?;
        Ok(serde_json::from_value(value)?)
    }

    /// Get one session.
    pub async fn session_get(
        &self,
        params: SessionGetParams,
    ) -> Result<SessionGetResult, SdkError> {
        let value = self
            .request(method::SESSION_GET, serde_json::to_value(params)?)
            .await?;
        Ok(serde_json::from_value(value)?)
    }

    /// Rename a session.
    pub async fn session_rename(
        &self,
        params: SessionRenameParams,
    ) -> Result<SessionRenameResult, SdkError> {
        let value = self
            .request(method::SESSION_RENAME, serde_json::to_value(params)?)
            .await?;
        Ok(serde_json::from_value(value)?)
    }

    /// Archive a session.
    pub async fn session_archive(
        &self,
        params: SessionArchiveParams,
    ) -> Result<SessionArchiveResult, SdkError> {
        let value = self
            .request(method::SESSION_ARCHIVE, serde_json::to_value(params)?)
            .await?;
        Ok(serde_json::from_value(value)?)
    }

    /// Soft-delete a session.
    pub async fn session_delete(
        &self,
        params: SessionDeleteParams,
    ) -> Result<SessionDeleteResult, SdkError> {
        let value = self
            .request(method::SESSION_DELETE, serde_json::to_value(params)?)
            .await?;
        Ok(serde_json::from_value(value)?)
    }

    /// Release a session's workspace lease.
    pub async fn session_release_lease(
        &self,
        params: SessionReleaseLeaseParams,
    ) -> Result<SessionReleaseLeaseResult, SdkError> {
        let value = self
            .request(method::SESSION_RELEASE_LEASE, serde_json::to_value(params)?)
            .await?;
        Ok(serde_json::from_value(value)?)
    }

    /// Resume a session.
    pub async fn session_resume(
        &self,
        params: SessionResumeParams,
    ) -> Result<SessionResumeResult, SdkError> {
        let value = self
            .request(method::SESSION_RESUME, serde_json::to_value(params)?)
            .await?;
        Ok(serde_json::from_value(value)?)
    }

    /// Change a role model.
    pub async fn session_set_model(
        &self,
        params: SessionSetModelParams,
    ) -> Result<SessionSetModelResult, SdkError> {
        let value = self
            .request(method::SESSION_SET_MODEL, serde_json::to_value(params)?)
            .await?;
        Ok(serde_json::from_value(value)?)
    }

    /// Search session history.
    pub async fn history_search(
        &self,
        params: HistorySearchParams,
    ) -> Result<HistorySearchResult, SdkError> {
        let value = self
            .request(method::HISTORY_SEARCH, serde_json::to_value(params)?)
            .await?;
        Ok(serde_json::from_value(value)?)
    }

    /// List models.
    pub async fn models_list(&self) -> Result<ModelsListResult, SdkError> {
        let value = self
            .request(method::MODELS_LIST, serde_json::json!({}))
            .await?;
        Ok(serde_json::from_value(value)?)
    }

    /// Send input to a session.
    pub async fn session_send(
        &self,
        params: SessionSendParams,
    ) -> Result<SessionSendResult, SdkError> {
        let value = self
            .request(method::SESSION_SEND, serde_json::to_value(params)?)
            .await?;
        Ok(serde_json::from_value(value)?)
    }

    /// Cancel a session turn.
    pub async fn session_cancel(
        &self,
        params: SessionCancelParams,
    ) -> Result<SessionCancelResult, SdkError> {
        let value = self
            .request(method::SESSION_CANCEL, serde_json::to_value(params)?)
            .await?;
        Ok(serde_json::from_value(value)?)
    }

    /// Approve or deny a pending action.
    pub async fn session_approve(
        &self,
        params: SessionApproveParams,
    ) -> Result<SessionApproveResult, SdkError> {
        let value = self
            .request(method::SESSION_APPROVE, serde_json::to_value(params)?)
            .await?;
        Ok(serde_json::from_value(value)?)
    }

    /// Subscribe to a session's event stream.
    pub async fn session_subscribe(
        &self,
        params: SessionSubscribeParams,
    ) -> Result<SessionSubscribeResult, SdkError> {
        let value = self
            .request(method::SESSION_SUBSCRIBE, serde_json::to_value(params)?)
            .await?;
        Ok(serde_json::from_value(value)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;
    use futures_util::stream;
    use std::sync::Mutex;
    use thais_protocol::jsonrpc::Id;
    use thais_protocol::session::SessionMode;

    struct FakeTransport {
        responses: Mutex<Vec<Response>>,
        requests: Mutex<Vec<Request>>,
    }

    impl FakeTransport {
        fn new(responses: Vec<Response>) -> Self {
            Self {
                responses: Mutex::new(responses),
                requests: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl Transport for FakeTransport {
        async fn send(&self, request: Request) -> Result<Response, SdkError> {
            self.requests.lock().expect("requests").push(request);
            Ok(self.responses.lock().expect("responses").remove(0))
        }

        fn notifications(&self) -> BoxStream<'static, Notification> {
            stream::empty().boxed()
        }
    }

    #[tokio::test]
    async fn typed_calls_build_requests_and_parse_results() {
        let response = Response::success(
            Id::Number(1),
            serde_json::to_value(InitializeResult {
                server_name: "thais".to_owned(),
                server_version: "0.1.0".to_owned(),
                protocol_version: "1.0".to_owned(),
                capabilities: serde_json::json!({}),
            })
            .expect("result"),
        );
        let transport = FakeTransport::new(vec![response]);
        let client = Client::new(transport);

        let result = client
            .initialize(InitializeParams {
                client_name: "test".to_owned(),
                client_version: "0.1".to_owned(),
                protocol_version: "1.0".to_owned(),
                capabilities: serde_json::json!({}),
            })
            .await
            .expect("initialize");
        assert_eq!(result.server_name, "thais");

        let requests = client.transport.requests.lock().expect("requests");
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, method::INITIALIZE);
    }

    #[tokio::test]
    async fn remote_errors_are_preserved() {
        let transport = FakeTransport::new(vec![Response::error(
            Id::Number(1),
            thais_protocol::error::ErrorObject::new(-32602, "bad params"),
        )]);
        let client = Client::new(transport);
        let error = client
            .session_create(SessionCreateParams {
                workspace: "/tmp".to_owned(),
                name: None,
                parent_id: None,
                mode: SessionMode::Build,
                model: None,
                steal_lease: false,
            })
            .await
            .expect_err("error");
        assert!(matches!(error, SdkError::Remote { code: -32602, .. }));
    }
}

#[cfg(test)]
mod channel_tests {
    use super::*;
    use futures_util::StreamExt;
    use thais_protocol::jsonrpc::{Message, Notification, Response};
    use thais_protocol::method;

    #[tokio::test]
    async fn channel_transport_round_trips_responses_and_notifications() {
        let (client, mut server) = channel::pair();
        let notification = Notification::new(method::SESSION_EVENT, serde_json::json!({"seq": 1}));

        let server_task = tokio::spawn(async move {
            let request = server.recv().await.expect("request");
            server
                .send(Message::Response(Response::success(
                    request.id,
                    serde_json::json!({"ok": true}),
                )))
                .await
                .expect("response");
            server
                .send(Message::Notification(notification))
                .await
                .expect("notification");
        });

        let mut notifications = client.notifications();
        let response = client
            .request(thais_protocol::jsonrpc::Request::new(
                1_i64,
                method::INITIALIZE,
                serde_json::json!({}),
            ))
            .await
            .expect("request");
        assert_eq!(response.result.expect("result")["ok"], true);

        let received =
            tokio::time::timeout(std::time::Duration::from_secs(2), notifications.next())
                .await
                .expect("notification timeout")
                .expect("notification");
        assert_eq!(received.method, method::SESSION_EVENT);

        server_task.await.expect("server task");
    }
}
