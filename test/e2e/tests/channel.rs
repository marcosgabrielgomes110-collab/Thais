use std::sync::Arc;

use futures_util::StreamExt;
use serde_json::json;
use thais_core::clock::FixedClock;
use thais_core::context::ContextManager;
use thais_core::ids::SequenceIdGenerator;
use thais_core::runtime::Runtime;
use thais_protocol::event::EventKind;
use thais_protocol::jsonrpc::{Message, Notification};
use thais_protocol::method;
use thais_protocol::session::{
    InitializeParams, SessionCreateParams, SessionMode, SessionSendParams,
};
use thais_providers::Provider;
use thais_providers::fake::{FakeProvider, FakeScript, FakeStep};
use thais_sdk::{Client, channel};

#[tokio::test]
async fn sdk_client_drives_the_runtime_through_the_in_process_bridge() {
    let temp = tempfile::tempdir().expect("temp");
    let storage =
        thais_storage::StorageHandle::open(temp.path().join("thais.db")).expect("storage");
    let provider: Arc<dyn Provider> = Arc::new(FakeProvider::new(
        "fake",
        FakeScript {
            model: Some("fake-test".to_owned()),
            steps: vec![FakeStep::Text {
                text: "hello from the bridge".to_owned(),
            }],
        },
    ));
    let runtime = Arc::new(Runtime::new(
        storage,
        provider,
        Arc::new(FixedClock::new("2026-10-23T00:00:00Z")),
        Arc::new(SequenceIdGenerator::new("t")),
        ContextManager::empty(),
    ));

    let (client_transport, mut server) = channel::pair();
    let request_sender = server.sender();
    let event_sender = server.sender();
    let request_runtime = Arc::clone(&runtime);

    tokio::spawn(async move {
        while let Some(request) = server.recv().await {
            let outcome = request_runtime.handle_request(request).await;
            if request_sender
                .send(Message::Response(outcome.response))
                .await
                .is_err()
            {
                break;
            }
            for event in outcome.replay {
                let notification =
                    Notification::new(method::SESSION_EVENT, json!({"event": event}));
                if request_sender
                    .send(Message::Notification(notification))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        }
    });

    let mut live = runtime.subscribe_live();
    tokio::spawn(async move {
        loop {
            match live.recv().await {
                Ok(event) => {
                    let notification =
                        Notification::new(method::SESSION_EVENT, json!({"event": event}));
                    if event_sender
                        .send(Message::Notification(notification))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => break,
            }
        }
    });

    let client = Client::new(client_transport);
    let mut notifications = client.notifications();

    client
        .initialize(InitializeParams {
            client_name: "bridge-test".to_owned(),
            client_version: "0.1".to_owned(),
            protocol_version: thais_protocol::PROTOCOL_VERSION.to_owned(),
            capabilities: json!({}),
        })
        .await
        .expect("initialize");

    let session = client
        .session_create(SessionCreateParams {
            workspace: temp.path().to_string_lossy().into_owned(),
            name: None,
            parent_id: None,
            mode: SessionMode::Build,
            model: None,
            steal_lease: false,
        })
        .await
        .expect("session")
        .session;

    client
        .session_send(SessionSendParams {
            session_id: session.session_id.clone(),
            input: "hello".to_owned(),
            turn_id: None,
            model: None,
        })
        .await
        .expect("send");

    let mut content = String::new();
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let notification = notifications.next().await.expect("notification");
            if notification.method != method::SESSION_EVENT {
                continue;
            }
            let event: thais_protocol::event::EventEnvelope =
                serde_json::from_value(notification.params["event"].clone()).expect("event");
            if event.session_id != session.session_id {
                continue;
            }
            match event.kind {
                EventKind::ModelDelta => {
                    content.push_str(event.payload["text"].as_str().unwrap_or_default());
                }
                EventKind::TurnCompleted => break,
                _ => {}
            }
        }
    })
    .await
    .expect("turn completed");

    assert_eq!(content, "hello from the bridge");
}
