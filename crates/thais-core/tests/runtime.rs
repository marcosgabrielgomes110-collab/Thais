use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use tempfile::TempDir;
use thais_core::clock::FixedClock;
use thais_core::context::ContextManager;
use thais_core::ids::SequenceIdGenerator;
use thais_core::runtime::{Runtime, RuntimeError};
use thais_protocol::event::EventKind;
use thais_protocol::jsonrpc::Request;
use thais_protocol::method;
use thais_protocol::session::{
    ApprovalDecision, InitializeResult, SessionCreateResult, SessionMode,
};
use thais_providers::fake::{FakeProvider, FakeScript, FakeStep};

fn script(steps: Vec<FakeStep>) -> FakeScript {
    FakeScript {
        model: Some("fake-test".to_owned()),
        steps,
    }
}

fn runtime(temp: &TempDir, script: FakeScript) -> Arc<Runtime> {
    runtime_with_scripts(temp, vec![script])
}

fn runtime_with_scripts(temp: &TempDir, scripts: Vec<FakeScript>) -> Arc<Runtime> {
    runtime_with_options(temp, scripts, None)
}

fn runtime_with_options(
    temp: &TempDir,
    scripts: Vec<FakeScript>,
    verifier: Option<thais_core::Verifier>,
) -> Arc<Runtime> {
    let storage =
        thais_storage::StorageHandle::open(temp.path().join("thais.db")).expect("storage");
    let provider: Arc<dyn thais_providers::Provider> =
        Arc::new(FakeProvider::with_scripts("fake", scripts));
    let mut runtime = Runtime::new(
        storage,
        provider,
        Arc::new(FixedClock::new("2026-10-23T00:00:00Z")),
        Arc::new(SequenceIdGenerator::new("t")),
        ContextManager::empty(),
    )
    .with_snapshot_base(temp.path().join("snapshots"));
    if let Some(verifier) = verifier {
        runtime = runtime.with_verifier(verifier);
    }
    Arc::new(runtime)
}

#[tokio::test]
async fn run_once_emits_a_deterministic_event_stream() {
    let temp = tempfile::tempdir().expect("temp");
    let runtime = runtime(
        &temp,
        script(vec![
            FakeStep::Text {
                text: "hello".to_owned(),
            },
            FakeStep::Reasoning {
                text: "thinking".to_owned(),
            },
            FakeStep::Usage {
                input_tokens: 3,
                output_tokens: 5,
            },
        ]),
    );

    let (session, turn_id, events) = runtime
        .run_once(
            temp.path().to_string_lossy().into_owned(),
            "say hello".to_owned(),
            None,
        )
        .await
        .expect("run");

    assert_eq!(session.model, "fake-test");
    assert!(!turn_id.is_empty());
    let kinds: Vec<EventKind> = events.iter().map(|event| event.kind).collect();
    assert_eq!(
        kinds,
        vec![
            EventKind::SessionCreated,
            EventKind::TurnStarted,
            EventKind::ModelDelta,
            EventKind::ReasoningDelta,
            EventKind::TurnCompleted,
        ]
    );

    let completed = events.last().expect("completion");
    assert_eq!(completed.payload["turn_id"], turn_id);
    assert_eq!(completed.payload["usage"]["input_tokens"], 3);
    assert_eq!(completed.payload["usage"]["output_tokens"], 5);
    assert!(
        events
            .iter()
            .all(|event| event.timestamp == "2026-10-23T00:00:00Z")
    );
}

#[tokio::test]
async fn cancel_interrupts_a_slow_turn() {
    let temp = tempfile::tempdir().expect("temp");
    let runtime = runtime(
        &temp,
        script(vec![
            FakeStep::Text {
                text: "first".to_owned(),
            },
            FakeStep::Delay { millis: 5_000 },
            FakeStep::Text {
                text: "never".to_owned(),
            },
        ]),
    );

    let mut live = runtime.subscribe_live();
    let session = runtime
        .create_session(
            temp.path().to_string_lossy().into_owned(),
            SessionMode::Build,
            None,
        )
        .await
        .expect("session");
    let turn_id = runtime
        .send(&session.session_id, "slow".to_owned(), None)
        .await
        .expect("send");

    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let event = live.recv().await.expect("event");
            if event.session_id != session.session_id {
                continue;
            }
            if event.kind == EventKind::ModelDelta {
                break;
            }
        }
    })
    .await
    .expect("saw first delta");

    assert!(
        runtime
            .cancel(&session.session_id, Some(&turn_id))
            .await
            .expect("cancel")
    );

    let cancelled = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let event = live.recv().await.expect("event");
            if event.session_id == session.session_id && event.kind == EventKind::TurnCancelled {
                break event;
            }
        }
    })
    .await
    .expect("cancelled");
    assert_eq!(cancelled.payload["turn_id"], turn_id);
}

#[tokio::test]
async fn approval_round_trip_resolves_the_waiter_and_logs_both_events() {
    let temp = tempfile::tempdir().expect("temp");
    let runtime = runtime(
        &temp,
        script(vec![FakeStep::Text {
            text: "ok".to_owned(),
        }]),
    );
    let mut live = runtime.subscribe_live();
    let session = runtime
        .create_session(
            temp.path().to_string_lossy().into_owned(),
            SessionMode::Build,
            None,
        )
        .await
        .expect("session");

    let waiting = {
        let runtime = Arc::clone(&runtime);
        let session_id = session.session_id.clone();
        tokio::spawn(async move {
            runtime
                .request_approval(&session_id, json!({"tool": "fake.echo"}))
                .await
        })
    };

    let approval_id = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let event = live.recv().await.expect("event");
            if event.kind == EventKind::PermissionRequested {
                break event.payload["approval_id"]
                    .as_str()
                    .expect("approval id")
                    .to_owned();
            }
        }
    })
    .await
    .expect("permission requested");

    assert!(
        runtime
            .approve(&session.session_id, &approval_id, ApprovalDecision::Allow)
            .await
            .expect("approve")
    );
    assert_eq!(
        waiting.await.expect("join").expect("decision"),
        ApprovalDecision::Allow
    );

    let resolved = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let event = live.recv().await.expect("event");
            if event.kind == EventKind::PermissionResolved {
                break event;
            }
        }
    })
    .await
    .expect("permission resolved");
    assert_eq!(resolved.payload["approval_id"], approval_id);
    assert_eq!(resolved.payload["decision"], "allow");
}

#[tokio::test]
async fn subscribe_replays_events_after_a_sequence() {
    let temp = tempfile::tempdir().expect("temp");
    let runtime = runtime(
        &temp,
        script(vec![FakeStep::Text {
            text: "hello".to_owned(),
        }]),
    );
    let session = runtime
        .create_session(
            temp.path().to_string_lossy().into_owned(),
            SessionMode::Build,
            None,
        )
        .await
        .expect("session");
    runtime
        .send(&session.session_id, "hello".to_owned(), None)
        .await
        .expect("send");

    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let sessions = runtime.list_sessions().await.expect("sessions");
            if sessions[0].session_id == session.session_id {
                let (_, _, replay) = runtime
                    .subscribe(&session.session_id, Some(0))
                    .await
                    .expect("subscribe");
                if replay
                    .iter()
                    .any(|event| event.kind == EventKind::TurnCompleted)
                {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("turn completed");

    let (subscription_id, first, replay) = runtime
        .subscribe(&session.session_id, Some(1))
        .await
        .expect("subscribe");
    assert!(!subscription_id.is_empty());
    assert_eq!(first, 2);
    assert!(replay.iter().all(|event| event.seq >= 2));
    assert!(!replay.is_empty());
}

#[tokio::test]
async fn initialize_handshake_and_unknown_method() {
    let temp = tempfile::tempdir().expect("temp");
    let runtime = runtime(&temp, script(vec![]));

    let request = Request::new(
        1_i64,
        method::INITIALIZE,
        json!({
            "client_name": "test",
            "client_version": "0.1",
            "protocol_version": "1.0",
            "capabilities": {}
        }),
    );
    let outcome = runtime.handle_request(request).await;
    let result: InitializeResult = outcome.response.result_as().expect("result");
    assert_eq!(result.server_name, "thais");
    assert_eq!(result.protocol_version, "1.0");

    let request = Request::new(2_i64, "not.a.method", json!({}));
    let outcome = runtime.handle_request(request).await;
    let error = outcome.response.error.expect("error");
    assert_eq!(error.code, -32601);
}

#[tokio::test]
async fn session_create_and_send_through_requests() {
    let temp = tempfile::tempdir().expect("temp");
    let runtime = runtime(
        &temp,
        script(vec![FakeStep::Text {
            text: "from request".to_owned(),
        }]),
    );
    let create = Request::new(
        1_i64,
        method::SESSION_CREATE,
        json!({"workspace": temp.path(), "mode": "build", "model": "fake-test"}),
    );
    let outcome = runtime.handle_request(create).await;
    let result: SessionCreateResult = outcome.response.result_as().expect("create result");
    assert_eq!(result.session.model, "fake-test");

    let send = Request::new(
        2_i64,
        method::SESSION_SEND,
        json!({"session_id": result.session.session_id, "input": "hello"}),
    );
    let outcome = runtime.handle_request(send).await;
    assert!(
        outcome.response.error.is_none(),
        "send failed: {:?}",
        outcome.response
    );

    let missing = Request::new(
        3_i64,
        method::SESSION_CREATE,
        json!({"workspace": temp.path().join("missing")}),
    );
    let outcome = runtime.handle_request(missing).await;
    let error = outcome.response.error.expect("error");
    assert_eq!(error.code, -32602);
}

#[tokio::test]
async fn send_rejects_a_second_turn_while_one_is_active() {
    let temp = tempfile::tempdir().expect("temp");
    let runtime = runtime(&temp, script(vec![FakeStep::Delay { millis: 5_000 }]));
    let session = runtime
        .create_session(
            temp.path().to_string_lossy().into_owned(),
            SessionMode::Build,
            None,
        )
        .await
        .expect("session");
    runtime
        .send(&session.session_id, "first".to_owned(), None)
        .await
        .expect("first");
    let second = runtime
        .send(&session.session_id, "second".to_owned(), None)
        .await;
    assert!(matches!(second, Err(RuntimeError::TurnInProgress(_))));
    let _ = runtime.cancel(&session.session_id, None).await;
}

#[tokio::test]
async fn tool_call_loop_reads_a_file_and_returns_the_result() {
    let temp = tempfile::tempdir().expect("temp");
    std::fs::write(
        temp.path().join("Cargo.toml"),
        "[package]\nname = \"demo\"\n",
    )
    .expect("cargo");
    let runtime = runtime_with_scripts(
        &temp,
        vec![
            script(vec![FakeStep::ToolCall {
                index: 0,
                id: "call_1".to_owned(),
                name: "read".to_owned(),
                arguments: json!({"path": "Cargo.toml"}).to_string(),
            }]),
            script(vec![FakeStep::Text {
                text: "done".to_owned(),
            }]),
        ],
    );

    let (_, _, events) = runtime
        .run_once(
            temp.path().to_string_lossy().into_owned(),
            "read Cargo.toml".to_owned(),
            None,
        )
        .await
        .expect("run");

    let kinds = events.iter().map(|event| event.kind).collect::<Vec<_>>();
    assert!(kinds.contains(&EventKind::ToolCall));
    assert!(kinds.contains(&EventKind::ToolResult));
    assert_eq!(events.last().expect("last").kind, EventKind::TurnCompleted);

    let result = events
        .iter()
        .find(|event| event.kind == EventKind::ToolResult)
        .expect("tool result");
    assert_eq!(result.payload["is_error"], false);
    assert!(
        result.payload["content"]
            .as_str()
            .expect("content")
            .contains("name = \"demo\"")
    );
}

#[tokio::test]
async fn multiple_tool_calls_are_executed_in_one_iteration() {
    let temp = tempfile::tempdir().expect("temp");
    std::fs::write(
        temp.path().join("Cargo.toml"),
        "[package]\nname = \"demo\"\n",
    )
    .expect("cargo");
    std::fs::write(temp.path().join("README.md"), "# demo\n").expect("readme");
    let runtime = runtime_with_scripts(
        &temp,
        vec![
            script(vec![
                FakeStep::ToolCall {
                    index: 0,
                    id: "call_1".to_owned(),
                    name: "read".to_owned(),
                    arguments: json!({"path": "Cargo.toml"}).to_string(),
                },
                FakeStep::ToolCall {
                    index: 1,
                    id: "call_2".to_owned(),
                    name: "read".to_owned(),
                    arguments: json!({"path": "README.md"}).to_string(),
                },
            ]),
            script(vec![FakeStep::Text {
                text: "done".to_owned(),
            }]),
        ],
    );

    let (_, _, events) = runtime
        .run_once(
            temp.path().to_string_lossy().into_owned(),
            "read both".to_owned(),
            None,
        )
        .await
        .expect("run");

    assert_eq!(
        events
            .iter()
            .filter(|event| event.kind == EventKind::ToolCall)
            .count(),
        2
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.kind == EventKind::ToolResult)
            .count(),
        2
    );
}

#[tokio::test]
async fn sensitive_read_is_denied_by_the_user_and_logged() {
    let temp = tempfile::tempdir().expect("temp");
    std::fs::write(temp.path().join(".env"), "TOKEN=secret\n").expect("env");
    let runtime = runtime_with_scripts(
        &temp,
        vec![
            script(vec![FakeStep::ToolCall {
                index: 0,
                id: "call_1".to_owned(),
                name: "read".to_owned(),
                arguments: json!({"path": ".env"}).to_string(),
            }]),
            script(vec![FakeStep::Text {
                text: "done".to_owned(),
            }]),
        ],
    );

    let mut live = runtime.subscribe_live();
    let session = runtime
        .create_session(
            temp.path().to_string_lossy().into_owned(),
            SessionMode::Build,
            None,
        )
        .await
        .expect("session");
    runtime
        .send(&session.session_id, "read the env".to_owned(), None)
        .await
        .expect("send");

    let approval_id = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let event = live.recv().await.expect("event");
            if event.kind == EventKind::PermissionRequested {
                break event.payload["approval_id"]
                    .as_str()
                    .expect("approval id")
                    .to_owned();
            }
        }
    })
    .await
    .expect("permission requested");

    assert!(
        runtime
            .approve(&session.session_id, &approval_id, ApprovalDecision::Deny)
            .await
            .expect("deny")
    );

    let result = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let event = live.recv().await.expect("event");
            if event.kind == EventKind::ToolResult {
                break event;
            }
        }
    })
    .await
    .expect("tool result");
    assert_eq!(result.payload["is_error"], true);
    assert_eq!(result.payload["content"], "denied by user");

    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let event = live.recv().await.expect("event");
            if event.kind == EventKind::TurnCompleted {
                break;
            }
        }
    })
    .await
    .expect("turn completed");
}

#[tokio::test]
async fn replay_reconstructs_the_transcript_from_the_event_log() {
    let temp = tempfile::tempdir().expect("temp");
    let runtime = runtime(
        &temp,
        script(vec![FakeStep::Text {
            text: "replayed hello".to_owned(),
        }]),
    );
    let (session, _, events) = runtime
        .run_once(
            temp.path().to_string_lossy().into_owned(),
            "hello".to_owned(),
            None,
        )
        .await
        .expect("run");

    let replay = runtime
        .replay_session(&session.session_id)
        .await
        .expect("replay");
    assert_eq!(replay.transcript, "replayed hello");
    assert_eq!(replay.last_seq, events.last().expect("last").seq);
    assert_eq!(replay.events.len(), events.len());
}

#[tokio::test]
async fn write_requires_approval_and_reports_a_diff() {
    let temp = tempfile::tempdir().expect("temp");
    let runtime = runtime_with_scripts(
        &temp,
        vec![
            script(vec![FakeStep::ToolCall {
                index: 0,
                id: "call_1".to_owned(),
                name: "write".to_owned(),
                arguments: json!({"path": "hello.txt", "content": "hello\n"}).to_string(),
            }]),
            script(vec![FakeStep::Text {
                text: "done".to_owned(),
            }]),
        ],
    );
    let mut live = runtime.subscribe_live();
    let session = runtime
        .create_session(
            temp.path().to_string_lossy().into_owned(),
            SessionMode::Build,
            None,
        )
        .await
        .expect("session");
    runtime
        .send(&session.session_id, "write a file".to_owned(), None)
        .await
        .expect("send");

    let mut kinds = Vec::new();
    let mut tool_result = None;
    tokio::time::timeout(Duration::from_secs(4), async {
        loop {
            let event = live.recv().await.expect("event");
            if event.session_id != session.session_id {
                continue;
            }
            kinds.push(event.kind);
            match event.kind {
                EventKind::PermissionRequested => {
                    let action = &event.payload["action"];
                    assert_eq!(action["tool"], "write");
                    assert!(action["diff"].as_str().unwrap_or("").contains("+hello"));
                    runtime
                        .approve(
                            &session.session_id,
                            event.payload["approval_id"].as_str().expect("approval id"),
                            ApprovalDecision::Allow,
                        )
                        .await
                        .expect("approve");
                }
                EventKind::ToolResult => tool_result = Some(event),
                EventKind::TurnCompleted => break,
                _ => {}
            }
        }
    })
    .await
    .expect("turn");

    assert!(kinds.contains(&EventKind::SnapshotCreated));
    assert!(kinds.contains(&EventKind::ToolCall));
    assert!(kinds.contains(&EventKind::ToolResult));
    let result = tool_result.expect("tool result");
    assert_eq!(result.payload["is_error"], false);
    assert!(
        result.payload["diff"]
            .as_str()
            .unwrap_or("")
            .contains("+hello")
    );
    assert_eq!(
        std::fs::read_to_string(temp.path().join("hello.txt")).expect("file"),
        "hello\n"
    );
}

#[tokio::test]
async fn failing_verifier_rolls_back_the_write() {
    let temp = tempfile::tempdir().expect("temp");
    std::fs::write(temp.path().join("file.txt"), "old\n").expect("file");
    let runtime = runtime_with_options(
        &temp,
        vec![
            script(vec![FakeStep::ToolCall {
                index: 0,
                id: "call_1".to_owned(),
                name: "write".to_owned(),
                arguments: json!({"path": "file.txt", "content": "new\n"}).to_string(),
            }]),
            script(vec![FakeStep::Text {
                text: "done".to_owned(),
            }]),
        ],
        Some(thais_core::Verifier::new(
            vec!["false".to_owned()],
            Duration::from_secs(5),
        )),
    );
    let mut live = runtime.subscribe_live();
    let session = runtime
        .create_session(
            temp.path().to_string_lossy().into_owned(),
            SessionMode::Build,
            None,
        )
        .await
        .expect("session");
    runtime
        .send(&session.session_id, "change the file".to_owned(), None)
        .await
        .expect("send");

    let mut kinds = Vec::new();
    let mut tool_result = None;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let event = live.recv().await.expect("event");
            if event.session_id != session.session_id {
                continue;
            }
            kinds.push(event.kind);
            match event.kind {
                EventKind::PermissionRequested => {
                    runtime
                        .approve(
                            &session.session_id,
                            event.payload["approval_id"].as_str().expect("approval id"),
                            ApprovalDecision::Allow,
                        )
                        .await
                        .expect("approve");
                }
                EventKind::ToolResult => tool_result = Some(event),
                EventKind::TurnCompleted => break,
                _ => {}
            }
        }
    })
    .await
    .expect("turn");

    assert!(kinds.contains(&EventKind::VerificationFailed));
    assert!(kinds.contains(&EventKind::RollbackCompleted));
    let result = tool_result.expect("tool result");
    assert_eq!(result.payload["is_error"], true);
    assert_eq!(
        std::fs::read_to_string(temp.path().join("file.txt")).expect("file"),
        "old\n"
    );
}

#[tokio::test]
async fn plan_mode_requires_approval_before_switching_to_build() {
    let temp = tempfile::tempdir().expect("temp");
    let runtime = runtime_with_scripts(
        &temp,
        vec![script(vec![FakeStep::Text {
            text: "# Plan\n1. Read the file.".to_owned(),
        }])],
    );
    let mut live = runtime.subscribe_live();
    let session = runtime
        .create_session(
            temp.path().to_string_lossy().into_owned(),
            SessionMode::Plan,
            None,
        )
        .await
        .expect("session");
    runtime
        .send(&session.session_id, "make a plan".to_owned(), None)
        .await
        .expect("send");

    tokio::time::timeout(Duration::from_secs(4), async {
        loop {
            let event = live.recv().await.expect("event");
            if event.session_id != session.session_id {
                continue;
            }
            if event.kind == EventKind::PermissionRequested {
                assert_eq!(event.payload["action"]["type"], "plan");
                runtime
                    .approve(
                        &session.session_id,
                        event.payload["approval_id"].as_str().expect("approval id"),
                        ApprovalDecision::Allow,
                    )
                    .await
                    .expect("approve");
            }
            if event.kind == EventKind::TurnCompleted {
                break;
            }
        }
    })
    .await
    .expect("plan approval");

    assert_eq!(
        runtime
            .get_session(&session.session_id)
            .await
            .expect("session")
            .mode,
        SessionMode::Build
    );
}
