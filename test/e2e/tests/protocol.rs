use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tempfile::TempDir;
use thais_core::clock::FixedClock;
use thais_core::ids::SequenceIdGenerator;
use thais_core::runtime::Runtime;
use thais_providers::Provider;
use thais_providers::fake::{FakeProvider, FakeScript, FakeStep};
use tokio::io::{
    AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, Lines, ReadHalf, WriteHalf,
};

struct Harness {
    _temp: TempDir,
    reader: Lines<BufReader<ReadHalf<DuplexStream>>>,
    writer: WriteHalf<DuplexStream>,
    server: tokio::task::JoinHandle<Result<(), thais_core::ServerError>>,
}

impl Harness {
    fn start() -> Self {
        let temp = tempfile::tempdir().expect("temp");
        let storage =
            thais_storage::StorageHandle::open(temp.path().join("thais.db")).expect("storage");
        let provider: Arc<dyn Provider> = Arc::new(FakeProvider::new(
            "fake",
            FakeScript {
                model: Some("fake-test".to_owned()),
                steps: vec![FakeStep::Text {
                    text: "hello from the fake".to_owned(),
                }],
            },
        ));
        let runtime = Arc::new(Runtime::new(
            storage,
            provider,
            Arc::new(FixedClock::new("2026-10-23T00:00:00Z")),
            Arc::new(SequenceIdGenerator::new("t")),
            thais_core::ContextManager::empty(),
        ));

        let (client, server) = tokio::io::duplex(64 * 1024);
        let (server_read, server_write) = tokio::io::split(server);
        let server = tokio::spawn(thais_core::serve(runtime, server_read, server_write));
        let (client_read, client_write) = tokio::io::split(client);

        Self {
            _temp: temp,
            reader: BufReader::new(client_read).lines(),
            writer: client_write,
            server,
        }
    }

    async fn send(&mut self, value: Value) {
        let mut line = serde_json::to_string(&value).expect("json");
        line.push('\n');
        self.writer.write_all(line.as_bytes()).await.expect("write");
        self.writer.flush().await.expect("flush");
    }

    async fn read(&mut self) -> Value {
        let line = tokio::time::timeout(Duration::from_secs(3), self.reader.next_line())
            .await
            .expect("read timed out")
            .expect("read")
            .expect("line");
        serde_json::from_str(&line).expect("json")
    }

    async fn request(&mut self, id: i64, method: &str, params: Value) -> Value {
        self.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        }))
        .await;

        loop {
            let message = self.read().await;
            if message.get("id").and_then(Value::as_i64) == Some(id) {
                return message;
            }
            assert_eq!(
                message["method"], "session.event",
                "unexpected message: {message}"
            );
        }
    }

    async fn wait_for_event(&mut self, kind: &str) -> Value {
        loop {
            let message = self.read().await;
            if message["method"] == "session.event" && message["params"]["event"]["kind"] == kind {
                return message["params"]["event"].clone();
            }
        }
    }

    async fn finish(mut self) {
        let _ = self.writer.shutdown().await;
        drop(self.writer);
        drop(self.reader);
        tokio::time::timeout(Duration::from_secs(3), self.server)
            .await
            .expect("server exit timed out")
            .expect("server join")
            .expect("server result");
    }
}

#[tokio::test]
async fn handshake_create_send_and_subscribe() {
    let mut harness = Harness::start();

    let response = harness
        .request(
            1,
            "initialize",
            json!({
                "client_name": "e2e",
                "client_version": "0.1",
                "protocol_version": "1.0",
                "capabilities": {}
            }),
        )
        .await;
    assert_eq!(response["result"]["protocol_version"], "1.0");
    assert_eq!(response["result"]["server_name"], "thais");

    let response = harness
        .request(
            2,
            "session.create",
            json!({"workspace": harness._temp.path(), "mode": "build"}),
        )
        .await;
    let session_id = response["result"]["session"]["session_id"]
        .as_str()
        .expect("session id")
        .to_owned();

    let response = harness
        .request(
            3,
            "session.send",
            json!({"session_id": session_id, "input": "hello"}),
        )
        .await;
    assert!(response["result"]["turn_id"].is_string());

    let completed = harness.wait_for_event("turn_completed").await;
    assert_eq!(
        completed["payload"]["turn_id"],
        response["result"]["turn_id"]
    );

    let response = harness
        .request(
            4,
            "session.subscribe",
            json!({"session_id": session_id, "from_seq": 0}),
        )
        .await;
    assert!(response["result"]["subscription_id"].is_string());

    // The response is followed by replay notifications.
    for _ in 0..4 {
        let message = harness.read().await;
        assert_eq!(message["method"], "session.event");
    }

    harness.finish().await;
}

#[tokio::test]
async fn unknown_method_and_parse_error() {
    let mut harness = Harness::start();

    let response = harness.request(1, "not.a.method", json!({})).await;
    assert_eq!(response["error"]["code"], -32601);

    harness
        .send(json!({"jsonrpc": "2.0", "id": 2, "method": "initialize"}))
        .await;
    // A request without params is valid but the missing protocol version is
    // allowed; the server responds successfully.
    let message = harness.read().await;
    assert_eq!(message["id"], 2);

    harness.finish().await;
}

#[tokio::test]
async fn malformed_json_returns_a_parse_error() {
    let mut harness = Harness::start();
    harness
        .writer
        .write_all(b"{not json}\n")
        .await
        .expect("write");
    harness.writer.flush().await.expect("flush");
    let response = harness.read().await;
    assert_eq!(response["id"], serde_json::Value::Null);
    assert_eq!(response["error"]["code"], -32700);
    harness.finish().await;
}
