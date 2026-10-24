//! In-process transport for tests and for the CLI-hosted TUI.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::stream::BoxStream;
use futures_util::{StreamExt, stream};
use thais_protocol::jsonrpc::{Id, Message, Notification, Request, Response};
use tokio::sync::{Mutex, broadcast, mpsc, oneshot};
use tokio_stream::wrappers::BroadcastStream;

use crate::{SdkError, Transport};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// Client half of an in-process transport pair.
pub struct ClientTransport {
    outgoing: mpsc::Sender<Request>,
    pending: Arc<Mutex<HashMap<Id, oneshot::Sender<Response>>>>,
    notifications: broadcast::Sender<Notification>,
}

/// Server half of an in-process transport pair.
pub struct ServerTransport {
    incoming: mpsc::Receiver<Request>,
    outgoing: mpsc::Sender<Message>,
}

impl ClientTransport {
    /// Send one request and wait for the matching response.
    pub async fn request(&self, request: Request) -> Result<Response, SdkError> {
        let id = request.id.clone();
        let (sender, receiver) = oneshot::channel();
        self.pending.lock().await.insert(id.clone(), sender);
        if self.outgoing.send(request).await.is_err() {
            self.pending.lock().await.remove(&id);
            return Err(SdkError::Transport("server closed".to_owned()));
        }
        tokio::time::timeout(REQUEST_TIMEOUT, receiver)
            .await
            .map_err(|_| SdkError::Transport("request timed out".to_owned()))?
            .map_err(|_| SdkError::Transport("response channel closed".to_owned()))
    }
}

impl ServerTransport {
    /// Receive the next request.
    pub async fn recv(&mut self) -> Option<Request> {
        self.incoming.recv().await
    }

    /// Send a message to the client.
    pub async fn send(&self, message: Message) -> Result<(), SdkError> {
        self.outgoing
            .send(message)
            .await
            .map_err(|_| SdkError::Transport("client closed".to_owned()))
    }

    /// Clone the outgoing message sender.
    #[must_use]
    pub fn sender(&self) -> mpsc::Sender<Message> {
        self.outgoing.clone()
    }
}

/// Create a connected client/server transport pair.
#[must_use]
pub fn pair() -> (ClientTransport, ServerTransport) {
    let (request_sender, request_receiver) = mpsc::channel(32);
    let (message_sender, mut message_receiver) = mpsc::channel(256);
    let pending: Arc<Mutex<HashMap<Id, oneshot::Sender<Response>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let (notification_sender, _) = broadcast::channel(512);

    let client = ClientTransport {
        outgoing: request_sender,
        pending: Arc::clone(&pending),
        notifications: notification_sender.clone(),
    };
    let server = ServerTransport {
        incoming: request_receiver,
        outgoing: message_sender,
    };

    tokio::spawn(async move {
        while let Some(message) = message_receiver.recv().await {
            match message {
                Message::Response(response) => {
                    if let Some(sender) = pending.lock().await.remove(&response.id) {
                        let _ = sender.send(response);
                    }
                }
                Message::Notification(notification) => {
                    let _ = notification_sender.send(notification);
                }
                Message::Request(_) => {}
            }
        }
    });

    (client, server)
}

#[async_trait]
impl Transport for ClientTransport {
    async fn send(&self, request: Request) -> Result<Response, SdkError> {
        self.request(request).await
    }

    fn notifications(&self) -> BoxStream<'static, Notification> {
        BroadcastStream::new(self.notifications.subscribe())
            .filter_map(|item| async move { item.ok() })
            .boxed()
    }
}

/// Empty notification stream for tests that do not need server pushes.
pub fn empty_notifications() -> BoxStream<'static, Notification> {
    stream::empty().boxed()
}
