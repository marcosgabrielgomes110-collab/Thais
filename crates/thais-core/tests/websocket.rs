use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use thais_core::clock::FixedClock;
use thais_core::ids::SequenceIdGenerator;
use thais_core::runtime::Runtime;
use thais_core::{ContextManager, serve_websocket_listener};
use thais_providers::fake::{FakeProvider, FakeScript, FakeStep};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

async fn runtime() -> Arc<Runtime> {
    let temp = tempfile::tempdir().expect("temp");
    let storage =
        thais_storage::StorageHandle::open(temp.path().join("db.sqlite")).expect("storage");
    // Leak the temp directory for the short test lifetime.
    std::mem::forget(temp);
    Arc::new(Runtime::new(
        storage,
        Arc::new(FakeProvider::new(
            "fake",
            FakeScript {
                model: Some("fake-test".to_owned()),
                steps: vec![FakeStep::Text {
                    text: "ok".to_owned(),
                }],
            },
        )),
        Arc::new(FixedClock::new("2026-10-23T00:00:00Z")),
        Arc::new(SequenceIdGenerator::new("w")),
        ContextManager::empty(),
    ))
}

#[tokio::test]
async fn websocket_requires_a_token_and_serves_requests() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("addr");
    let server = tokio::spawn(serve_websocket_listener(
        runtime().await,
        listener,
        "secret".to_owned(),
    ));

    let missing = tokio_tungstenite::connect_async(format!("ws://{address}/")).await;
    assert!(missing.is_err());

    let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{address}/?token=secret"))
        .await
        .expect("connect");
    socket
        .send(Message::Text(
            json!({
                "jsonrpc":"2.0","id":1,"method":"initialize",
                "params":{"client_name":"test","client_version":"1","protocol_version":"1.0","capabilities":{}}
            })
            .to_string()
            .into(),
        ))
        .await
        .expect("send");
    let response = tokio::time::timeout(Duration::from_secs(2), socket.next())
        .await
        .expect("timeout")
        .expect("message")
        .expect("ws");
    let value: serde_json::Value =
        serde_json::from_str(&response.into_text().expect("text")).expect("json");
    assert_eq!(value["result"]["server_name"], "thais");

    server.abort();
}
