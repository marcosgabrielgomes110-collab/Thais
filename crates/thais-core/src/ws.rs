//! Local WebSocket daemon transport.

use std::net::SocketAddr;
use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use thais_protocol::error::{ErrorObject, PARSE_ERROR};
use thais_protocol::jsonrpc::{Id, Message, Notification, Response};
use thais_protocol::method;
use tokio::net::TcpListener;
use tokio_tungstenite::accept_hdr_async;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::handshake::server::{
    ErrorResponse, Request, Response as HandshakeResponse,
};

use crate::runtime::Runtime;
use crate::server::ServerError;

/// Serve the Agent API over WebSocket on localhost with a required token.
///
/// The token is accepted as `?token=...` on the WebSocket URL. The server
/// refuses requests without it.
pub async fn serve_websocket(
    runtime: Arc<Runtime>,
    addr: SocketAddr,
    token: String,
) -> Result<(), ServerError> {
    let listener = TcpListener::bind(addr).await?;
    let bound = listener.local_addr()?;
    tracing::info!(%bound, "Thais WebSocket daemon listening");
    serve_websocket_listener(runtime, listener, token).await
}

/// Serve on an existing listener. Useful for tests and daemon supervisors.
pub async fn serve_websocket_listener(
    runtime: Arc<Runtime>,
    listener: TcpListener,
    token: String,
) -> Result<(), ServerError> {
    loop {
        let (stream, peer) = listener.accept().await?;
        let runtime = Arc::clone(&runtime);
        let token = token.clone();
        tokio::spawn(async move {
            if let Err(error) = handle_connection(stream, runtime, token).await {
                tracing::debug!(%peer, "websocket connection ended: {error}");
            }
        });
    }
}

#[allow(clippy::result_large_err)]
async fn handle_connection(
    stream: tokio::net::TcpStream,
    runtime: Arc<Runtime>,
    token: String,
) -> Result<(), ServerError> {
    let callback = |request: &Request, response: HandshakeResponse| {
        let query = request.uri().query().unwrap_or_default();
        let authorized = query.split('&').any(|part| {
            let mut parts = part.splitn(2, '=');
            parts.next() == Some("token") && parts.next() == Some(token.as_str())
        });
        if authorized {
            Ok(response)
        } else {
            let mut denied: ErrorResponse = ErrorResponse::new(None);
            *denied.status_mut() = tokio_tungstenite::tungstenite::http::StatusCode::UNAUTHORIZED;
            Err(denied)
        }
    };
    let websocket = accept_hdr_async(stream, callback)
        .await
        .map_err(|error| ServerError::Io(std::io::Error::other(error.to_string())))?;
    let (mut sink, mut source) = websocket.split();
    let (outgoing, mut outgoing_receiver) = tokio::sync::mpsc::channel::<String>(256);
    let writer = tokio::spawn(async move {
        while let Some(text) = outgoing_receiver.recv().await {
            if sink.send(WsMessage::Text(text.into())).await.is_err() {
                break;
            }
        }
    });

    let mut live = runtime.subscribe_live();
    let event_sender = outgoing.clone();
    let event_task = tokio::spawn(async move {
        loop {
            match live.recv().await {
                Ok(event) => {
                    let notification = Notification::new(
                        method::SESSION_EVENT,
                        serde_json::json!({"event": event}),
                    );
                    if let Ok(text) = serde_json::to_string(&notification)
                        && event_sender.send(text).await.is_err()
                    {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => break,
            }
        }
    });

    while let Some(message) = source.next().await {
        let message =
            message.map_err(|error| ServerError::Io(std::io::Error::other(error.to_string())))?;
        let text = match message {
            WsMessage::Text(text) => text.to_string(),
            WsMessage::Binary(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            WsMessage::Close(_) => break,
            _ => continue,
        };
        let mut response = Vec::new();
        match serde_json::from_str::<Message>(&text) {
            Ok(Message::Request(request)) => {
                let outcome = runtime.handle_request(request).await;
                response.push(serde_json::to_string(&outcome.response)?);
                for event in outcome.replay {
                    response.push(serde_json::to_string(&Notification::new(
                        method::SESSION_EVENT,
                        serde_json::json!({"event": event}),
                    ))?);
                }
            }
            Ok(Message::Notification(_) | Message::Response(_)) => {}
            Err(error) => {
                response.push(serde_json::to_string(&Response::error(
                    Id::Null,
                    ErrorObject::new(PARSE_ERROR, error.to_string()),
                ))?);
            }
        }
        for text in response {
            outgoing.send(text).await.map_err(|_| ServerError::Closed)?;
        }
    }

    event_task.abort();
    let _ = event_task.await;
    drop(outgoing);
    let _ = writer.await;
    Ok(())
}
