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
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

async fn send(socket: &mut Ws, value: serde_json::Value) {
    socket
        .send(Message::Text(value.to_string().into()))
        .await
        .expect("send");
}

async fn recv(socket: &mut Ws) -> serde_json::Value {
    let message = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .expect("timeout")
        .expect("message")
        .expect("websocket");
    serde_json::from_str(&message.into_text().expect("text")).expect("json")
}

async fn response(socket: &mut Ws, id: i64) -> serde_json::Value {
    for _ in 0..32 {
        let value = recv(socket).await;
        if value["id"] == id {
            return value;
        }
    }
    panic!("response {id} not received");
}

async fn initialize(socket: &mut Ws, id: i64) {
    send(
        socket,
        json!({
            "jsonrpc":"2.0","id":id,"method":"initialize",
            "params":{"client_name":"test","client_version":"1","protocol_version":"1.0","capabilities":{}}
        }),
    )
    .await;
    assert_eq!(response(socket, id).await["result"]["server_name"], "thais");
}

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

#[tokio::test]
async fn two_clients_receive_the_same_session_events() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("addr");
    let server = tokio::spawn(serve_websocket_listener(
        runtime().await,
        listener,
        "secret".to_owned(),
    ));

    let url = format!("ws://{address}/?token=secret");
    let (mut first, _) = tokio_tungstenite::connect_async(&url).await.expect("first");
    let (mut second, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("second");
    initialize(&mut first, 1).await;
    initialize(&mut second, 1).await;

    send(
        &mut first,
        json!({
            "jsonrpc":"2.0","id":2,"method":"session.create",
            "params":{"workspace":"/tmp","mode":"build"}
        }),
    )
    .await;
    let created = response(&mut first, 2).await;
    let session_id = created["result"]["session"]["session_id"]
        .as_str()
        .expect("session id")
        .to_owned();

    send(
        &mut second,
        json!({
            "jsonrpc":"2.0","id":2,"method":"session.subscribe",
            "params":{"session_id":session_id,"from_seq":0}
        }),
    )
    .await;
    let _ = response(&mut second, 2).await;

    send(
        &mut first,
        json!({
            "jsonrpc":"2.0","id":3,"method":"session.send",
            "params":{"session_id":session_id,"input":"hello"}
        }),
    )
    .await;

    let mut saw_turn_completed = false;
    for _ in 0..32 {
        let value = recv(&mut second).await;
        if value["method"] == "session.event"
            && value["params"]["event"]["session_id"] == session_id
            && value["params"]["event"]["kind"] == "turn_completed"
        {
            saw_turn_completed = true;
            break;
        }
    }
    assert!(saw_turn_completed, "second client missed turn_completed");
    let _ = response(&mut first, 3).await;
    server.abort();
}
