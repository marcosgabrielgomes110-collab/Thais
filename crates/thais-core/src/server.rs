//! NDJSON JSON-RPC server over stdio.

use std::sync::Arc;

use thais_protocol::error::{ErrorObject, PARSE_ERROR};
use thais_protocol::jsonrpc::{Id, Message, Notification, Response};
use thais_protocol::method;
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

use crate::runtime::Runtime;

/// Server transport errors.
#[derive(Debug, Error)]
pub enum ServerError {
    /// I/O failure.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// JSON serialization failure.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    /// The output channel closed.
    #[error("output channel closed")]
    Closed,
    /// A background task failed.
    #[error("server task failed: {0}")]
    Join(String),
}

/// Serve the Agent API over stdio using one JSON message per line.
///
/// `stdout` carries protocol messages only. Logs belong on `stderr`.
pub async fn serve_stdio(runtime: Arc<Runtime>) -> Result<(), ServerError> {
    serve(runtime, tokio::io::stdin(), tokio::io::stdout()).await
}

/// Serve the Agent API over any async reader/writer pair with NDJSON framing.
pub async fn serve<R, W>(runtime: Arc<Runtime>, reader: R, writer: W) -> Result<(), ServerError>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (sender, mut receiver) = mpsc::channel::<String>(256);

    let writer = tokio::spawn(async move {
        let mut writer = writer;
        while let Some(line) = receiver.recv().await {
            if writer.write_all(line.as_bytes()).await.is_err() {
                break;
            }
            if writer.write_all(b"\n").await.is_err() {
                break;
            }
            if writer.flush().await.is_err() {
                break;
            }
        }
    });

    let mut live = runtime.subscribe_live();
    let event_sender = sender.clone();
    let event_task = tokio::spawn(async move {
        loop {
            match live.recv().await {
                Ok(event) => {
                    let notification = Notification::new(
                        method::SESSION_EVENT,
                        serde_json::json!({"event": event}),
                    );
                    if let Ok(line) = serde_json::to_string(&notification)
                        && event_sender.send(line).await.is_err()
                    {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => break,
            }
        }
    });

    let mut lines = BufReader::new(reader).lines();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }

        let mut outgoing = Vec::new();
        match serde_json::from_str::<Message>(&line) {
            Ok(Message::Request(request)) => {
                let outcome = runtime.handle_request(request).await;
                outgoing.push(serde_json::to_string(&outcome.response)?);
                for event in outcome.replay {
                    let notification = Notification::new(
                        method::SESSION_EVENT,
                        serde_json::json!({"event": event}),
                    );
                    outgoing.push(serde_json::to_string(&notification)?);
                }
            }
            Ok(Message::Notification(_) | Message::Response(_)) => {}
            Err(error) => {
                let response =
                    Response::error(Id::Null, ErrorObject::new(PARSE_ERROR, error.to_string()));
                outgoing.push(serde_json::to_string(&response)?);
            }
        }

        for message in outgoing {
            sender
                .send(message)
                .await
                .map_err(|_| ServerError::Closed)?;
        }
    }

    event_task.abort();
    let _ = event_task.await;
    drop(sender);
    writer
        .await
        .map_err(|error| ServerError::Join(error.to_string()))?;
    Ok(())
}
