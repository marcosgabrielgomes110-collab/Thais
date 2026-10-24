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
    ApprovalDecision, HistorySearchParams, HistorySearchResult, InitializeResult, ModelsListResult,
    SessionCreateResult, SessionListResult, SessionMode, SessionResumeResult,
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

fn runtime_with_prefix(temp: &TempDir, script: FakeScript, prefix: &str) -> Arc<Runtime> {
    let storage =
        thais_storage::StorageHandle::open(temp.path().join("thais.db")).expect("storage");
    let provider: Arc<dyn thais_providers::Provider> =
        Arc::new(FakeProvider::with_scripts("fake", vec![script]));
    Arc::new(
        Runtime::new(
            storage,
            provider,
            Arc::new(FixedClock::new("2026-10-23T00:00:00Z")),
            Arc::new(SequenceIdGenerator::new(prefix)),
            ContextManager::empty(),
        )
        .with_snapshot_base(temp.path().join("snapshots"))
        .with_checkpoint_base(temp.path().join("sessions")),
    )
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
    .with_snapshot_base(temp.path().join("snapshots"))
    .with_checkpoint_base(temp.path().join("sessions"));
    if let Some(verifier) = verifier {
        runtime = runtime.with_verifier(verifier);
    }
    Arc::new(runtime)
}

fn runtime_with_rebuild(
    temp: &TempDir,
    scripts: Vec<FakeScript>,
    threshold: usize,
    tail_turns: usize,
) -> Arc<Runtime> {
    let storage =
        thais_storage::StorageHandle::open(temp.path().join("thais.db")).expect("storage");
    let provider: Arc<dyn thais_providers::Provider> =
        Arc::new(FakeProvider::with_scripts("fake", scripts));
    Arc::new(
        Runtime::new(
            storage,
            provider,
            Arc::new(FixedClock::new("2026-10-23T00:00:00Z")),
            Arc::new(SequenceIdGenerator::new("r")),
            ContextManager::empty(),
        )
        .with_snapshot_base(temp.path().join("snapshots"))
        .with_checkpoint_base(temp.path().join("sessions"))
        .with_rebuild_threshold(threshold)
        .with_history_tail_turns(tail_turns),
    )
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
    assert_eq!(completed.payload["session_usage"]["input_tokens"], 3);
    assert_eq!(completed.payload["session_usage"]["output_tokens"], 5);
    let stored = runtime
        .get_session(&session.session_id)
        .await
        .expect("stored session");
    assert_eq!(stored.input_tokens, 3);
    assert_eq!(stored.output_tokens, 5);
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
    let checkpoint = std::fs::read_to_string(
        temp.path()
            .join("sessions")
            .join(&session.session_id)
            .join("checkpoint.md"),
    )
    .expect("checkpoint");
    assert!(checkpoint.contains("Read the file."), "{checkpoint}");
}

#[tokio::test]
async fn session_lifecycle_and_checkpoint_survive_restarts() {
    let temp = tempfile::tempdir().expect("temp");
    let runtime = runtime(&temp, script(vec![]));
    let session = runtime
        .create_session(
            temp.path().to_string_lossy().into_owned(),
            SessionMode::Build,
            None,
        )
        .await
        .expect("create");

    let checkpoint = temp
        .path()
        .join("sessions")
        .join(&session.session_id)
        .join("checkpoint.md");
    assert!(checkpoint.exists(), "checkpoint should exist");
    let text = std::fs::read_to_string(&checkpoint).expect("checkpoint");
    assert!(text.contains(&session.session_id));

    let renamed = runtime
        .rename_session(&session.session_id, "renamed")
        .await
        .expect("rename");
    assert_eq!(renamed.name, "renamed");

    let list = runtime.list_sessions().await.expect("list");
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].name, "renamed");

    let archived = runtime
        .archive_session(&session.session_id)
        .await
        .expect("archive");
    assert_eq!(archived.status.as_str(), "archived");
    assert!(runtime.list_sessions().await.expect("list").is_empty());
    assert_eq!(
        runtime
            .list_sessions_with_archived(true)
            .await
            .expect("list all")
            .len(),
        1
    );

    let resumed = runtime
        .resume_session(&session.session_id)
        .await
        .expect("resume");
    assert_eq!(resumed.session.name, "renamed");
    assert!(resumed.checkpoint.is_some());

    assert!(
        runtime
            .delete_session(&session.session_id)
            .await
            .expect("delete")
    );
    assert!(
        runtime
            .list_sessions_with_archived(true)
            .await
            .expect("list")
            .is_empty()
    );
}

#[tokio::test]
async fn one_mutating_session_per_workspace_and_steal_requires_explicit_flag() {
    let temp = tempfile::tempdir().expect("temp");
    let first = runtime(&temp, script(vec![]));
    let second = runtime_with_prefix(&temp, script(vec![]), "u");
    let workspace = temp.path().to_string_lossy().into_owned();

    let owner = first
        .create_session(workspace.clone(), SessionMode::Build, None)
        .await
        .expect("owner");
    let error = second
        .create_session(workspace.clone(), SessionMode::Build, None)
        .await
        .expect_err("lease must block");
    assert!(
        matches!(error, RuntimeError::LeaseHeld(_)),
        "unexpected: {error:?}"
    );

    let stolen = second
        .create_session_with(
            workspace.clone(),
            SessionMode::Build,
            None,
            Some("steal".to_owned()),
            true,
        )
        .await
        .expect("steal");
    assert_eq!(stolen.name, "steal");

    let error = first
        .send(&owner.session_id, "hello".to_owned(), None)
        .await
        .expect_err("old owner lost the lease");
    assert!(matches!(error, RuntimeError::LeaseHeld(_)));

    // Planning sessions never take the mutating lease.
    second
        .create_session(workspace.clone(), SessionMode::Plan, None)
        .await
        .expect("plan");
    second
        .create_session(workspace, SessionMode::Plan, None)
        .await
        .expect("second plan");
}

#[tokio::test]
async fn pending_approvals_are_durable_across_runtime_instances() {
    let temp = tempfile::tempdir().expect("temp");
    let first = runtime(&temp, script(vec![]));
    let session = first
        .create_session(
            temp.path().to_string_lossy().into_owned(),
            SessionMode::Build,
            None,
        )
        .await
        .expect("create");
    let mut live = first.subscribe_live();
    let waiting = {
        let runtime = Arc::clone(&first);
        let session_id = session.session_id.clone();
        tokio::spawn(async move {
            runtime
                .request_approval(&session_id, json!({"type": "write", "path": "a.txt"}))
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

    let second = runtime(&temp, script(vec![]));
    let resumed = second
        .resume_session(&session.session_id)
        .await
        .expect("resume");
    assert!(
        resumed
            .pending_approvals
            .iter()
            .any(|approval| approval.approval_id == approval_id)
    );
    assert!(
        second
            .approve(&session.session_id, &approval_id, ApprovalDecision::Deny)
            .await
            .expect("approve durable")
    );
    let resumed = second
        .resume_session(&session.session_id)
        .await
        .expect("resume again");
    assert!(resumed.pending_approvals.is_empty());
    waiting.abort();
}

#[tokio::test]
async fn model_policy_and_catalog_are_exposed_through_the_api() {
    let temp = tempfile::tempdir().expect("temp");
    let runtime = runtime(&temp, script(vec![]));
    let session = runtime
        .create_session(
            temp.path().to_string_lossy().into_owned(),
            SessionMode::Build,
            None,
        )
        .await
        .expect("create");

    let catalog = runtime.list_models().await.expect("models");
    assert_eq!(catalog.len(), 1);
    assert_eq!(catalog[0].id, "fake-test");

    let updated = runtime
        .set_model(&session.session_id, "default", "fake-2")
        .await
        .expect("default");
    assert_eq!(updated.model_policy["default"], "fake-2");
    assert_eq!(updated.model, "fake-2");
    let updated = runtime
        .set_model(&session.session_id, "build", "fake-3")
        .await
        .expect("build");
    assert_eq!(updated.model_policy["roles"]["build"][0], "fake-3");

    let request = Request::new(9_i64, method::MODELS_LIST, json!({}));
    let outcome = runtime.handle_request(request).await;
    let result: ModelsListResult = outcome.response.result_as().expect("models result");
    assert_eq!(result.models[0].id, "fake-test");

    let request = Request::new(
        10_i64,
        method::SESSION_LIST,
        json!({"include_archived": true}),
    );
    let outcome = runtime.handle_request(request).await;
    let result: SessionListResult = outcome.response.result_as().expect("list result");
    assert_eq!(result.sessions.len(), 1);

    let request = Request::new(
        11_i64,
        method::SESSION_RESUME,
        json!({"session_id": session.session_id}),
    );
    let outcome = runtime.handle_request(request).await;
    let _: SessionResumeResult = outcome.response.result_as().expect("resume result");
}

#[tokio::test]
async fn per_turn_model_override_is_accepted_and_logged() {
    let temp = tempfile::tempdir().expect("temp");
    let runtime = runtime(
        &temp,
        script(vec![FakeStep::Text {
            text: "override".to_owned(),
        }]),
    );
    let session = runtime
        .create_session(
            temp.path().to_string_lossy().into_owned(),
            SessionMode::Build,
            None,
        )
        .await
        .expect("create");
    let mut live = runtime.subscribe_live();
    runtime
        .send_with_model(
            &session.session_id,
            "hello".to_owned(),
            None,
            Some("other-model".to_owned()),
        )
        .await
        .expect("send");
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let event = live.recv().await.expect("event");
            if event.session_id == session.session_id && event.kind == EventKind::TurnCompleted {
                break;
            }
        }
    })
    .await
    .expect("turn completed");
}

#[tokio::test]
async fn long_session_rebuilds_context_and_persists_a_summary() {
    let temp = tempfile::tempdir().expect("temp");
    let text = "context material ".repeat(40);
    let runtime = runtime_with_rebuild(
        &temp,
        vec![
            script(vec![FakeStep::Text { text: text.clone() }]),
            script(vec![FakeStep::Text { text: text.clone() }]),
            script(vec![FakeStep::Text { text: text.clone() }]),
        ],
        80,
        1,
    );
    let session = runtime
        .create_session(
            temp.path().to_string_lossy().into_owned(),
            SessionMode::Build,
            None,
        )
        .await
        .expect("create");

    for input in ["first task", "second task", "third task"] {
        let mut live = runtime.subscribe_live();
        runtime
            .send(&session.session_id, input.to_owned(), None)
            .await
            .expect("send");
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let event = live.recv().await.expect("event");
                if event.session_id == session.session_id
                    && matches!(event.kind, EventKind::TurnCompleted | EventKind::Error)
                {
                    break;
                }
            }
        })
        .await
        .expect("turn complete");
    }

    let replay = runtime
        .replay_session(&session.session_id)
        .await
        .expect("replay");
    assert!(
        replay
            .events
            .iter()
            .any(|event| event.kind == EventKind::ContextRebuilt),
        "expected a rebuild event"
    );
    assert!(
        replay
            .events
            .iter()
            .any(|event| event.kind == EventKind::ContextRebuildFailed),
        "expected a safe fallback before the first successful rebuild"
    );
    let checkpoint = std::fs::read_to_string(
        temp.path()
            .join("sessions")
            .join(&session.session_id)
            .join("checkpoint.md"),
    )
    .expect("checkpoint");
    assert!(checkpoint.contains("Compacted"), "{checkpoint}");
    let resumed = runtime
        .resume_session(&session.session_id)
        .await
        .expect("resume");
    assert!(
        resumed
            .checkpoint
            .and_then(|checkpoint| checkpoint.summary)
            .is_some(),
        "resume should expose the compacted summary"
    );
}

#[tokio::test]
async fn memory_write_is_gated_and_recorded() {
    let temp = tempfile::tempdir().expect("temp");
    let runtime = runtime(
        &temp,
        script(vec![FakeStep::ToolCall {
            index: 0,
            id: "call_memory".to_owned(),
            name: "memory_write".to_owned(),
            arguments: json!({"rule": "Always run make check"}).to_string(),
        }]),
    );
    let session = runtime
        .create_session(
            temp.path().to_string_lossy().into_owned(),
            SessionMode::Build,
            None,
        )
        .await
        .expect("create");
    let mut live = runtime.subscribe_live();
    runtime
        .send(&session.session_id, "remember this".to_owned(), None)
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
            .approve(&session.session_id, &approval_id, ApprovalDecision::Allow)
            .await
            .expect("approve")
    );
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let event = live.recv().await.expect("event");
            if event.kind == EventKind::MemoryUpdated {
                break;
            }
        }
    })
    .await
    .expect("memory updated");
    let memory = std::fs::read_to_string(temp.path().join("MEMORY.md")).expect("memory");
    assert!(memory.contains("Always run make check"));
    let replay = runtime
        .replay_session(&session.session_id)
        .await
        .expect("replay");
    assert!(replay.events.iter().any(|event| {
        event.kind == EventKind::PermissionResolved
            && event
                .payload
                .get("decision")
                .and_then(serde_json::Value::as_str)
                == Some("allow")
    }));
}

#[tokio::test]
async fn history_search_filters_sessions_and_survives_fts_rebuild() {
    let temp = tempfile::tempdir().expect("temp");
    let runtime = runtime(
        &temp,
        script(vec![FakeStep::Text {
            text: "unique decision phrase".to_owned(),
        }]),
    );
    let session = runtime
        .create_session(
            temp.path().to_string_lossy().into_owned(),
            SessionMode::Build,
            None,
        )
        .await
        .expect("create");
    let mut live = runtime.subscribe_live();
    runtime
        .send(
            &session.session_id,
            "remember the unique decision phrase".to_owned(),
            None,
        )
        .await
        .expect("send");
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let event = live.recv().await.expect("event");
            if event.session_id == session.session_id && event.kind == EventKind::TurnCompleted {
                break;
            }
        }
    })
    .await
    .expect("turn complete");
    let matches = runtime
        .history_search(HistorySearchParams {
            query: "unique".to_owned(),
            session_id: Some(session.session_id.clone()),
            workspace: Some(temp.path().to_string_lossy().into_owned()),
            limit: Some(10),
        })
        .await
        .expect("search");
    assert!(!matches.matches.is_empty());
    let storage =
        thais_storage::StorageHandle::open(temp.path().join("thais.db")).expect("storage");
    storage.rebuild_fts().await.expect("rebuild fts");
    let matches2 = runtime
        .history_search(HistorySearchParams {
            query: "unique".to_owned(),
            session_id: Some(session.session_id.clone()),
            workspace: None,
            limit: Some(10),
        })
        .await
        .expect("search after rebuild");
    assert_eq!(matches.matches.len(), matches2.matches.len());

    let other = runtime
        .create_session(
            temp.path().to_string_lossy().into_owned(),
            SessionMode::Plan,
            None,
        )
        .await
        .expect("other session");
    let isolated = runtime
        .history_search(HistorySearchParams {
            query: "unique".to_owned(),
            session_id: Some(other.session_id),
            workspace: None,
            limit: Some(10),
        })
        .await
        .expect("isolated search");
    assert!(isolated.matches.is_empty());

    let request = Request::new(
        12_i64,
        method::HISTORY_SEARCH,
        json!({"query": "unique", "session_id": session.session_id}),
    );
    let outcome = runtime.handle_request(request).await;
    let result: HistorySearchResult = outcome.response.result_as().expect("history result");
    assert!(!result.matches.is_empty());
    assert_eq!(result.matches[0].session_name, session.name);
}

#[tokio::test]
async fn releasing_a_lease_allows_the_next_mutating_session() {
    let temp = tempfile::tempdir().expect("temp");
    let runtime = runtime(&temp, script(vec![]));
    let workspace = temp.path().to_string_lossy().into_owned();
    let first = runtime
        .create_session(workspace.clone(), SessionMode::Build, None)
        .await
        .expect("first");
    assert!(
        runtime
            .release_session_lease(&first.session_id)
            .await
            .expect("release")
    );
    runtime
        .create_session(workspace, SessionMode::Build, None)
        .await
        .expect("second after release");
}

#[tokio::test]
async fn explore_subagent_runs_in_a_child_session_and_returns_a_summary() {
    let temp = tempfile::tempdir().expect("temp");
    let runtime = runtime_with_scripts(
        &temp,
        vec![
            script(vec![FakeStep::ToolCall {
                index: 0,
                id: "call_subagent".to_owned(),
                name: "subagent".to_owned(),
                arguments: json!({
                    "kind": "explore",
                    "task": "inspect the repository"
                })
                .to_string(),
            }]),
            script(vec![FakeStep::Text {
                text: "child summary: no writes".to_owned(),
            }]),
            script(vec![FakeStep::Text {
                text: "parent received the summary".to_owned(),
            }]),
        ],
    );
    let session = runtime
        .create_session(
            temp.path().to_string_lossy().into_owned(),
            SessionMode::Build,
            None,
        )
        .await
        .expect("create");
    let mut live = runtime.subscribe_live();
    runtime
        .send(&session.session_id, "delegate".to_owned(), None)
        .await
        .expect("send");
    let mut tool_result = None;
    tokio::time::timeout(Duration::from_secs(4), async {
        loop {
            let event = live.recv().await.expect("event");
            if event.session_id == session.session_id
                && event.kind == EventKind::ToolResult
                && event.payload["name"] == "subagent"
            {
                tool_result = Some(event.clone());
            }
            if event.session_id == session.session_id && event.kind == EventKind::TurnCompleted {
                break;
            }
        }
    })
    .await
    .expect("parent turn");
    let result = tool_result.expect("subagent result");
    assert!(
        result.payload["content"]
            .as_str()
            .expect("content")
            .contains("child summary"),
        "{result:?}"
    );
    let sessions = runtime
        .list_sessions_with_archived(true)
        .await
        .expect("list");
    assert!(
        sessions
            .iter()
            .any(|child| child.parent_id.as_deref() == Some(&session.session_id)),
        "child session should carry parent_id"
    );
    let events = runtime
        .replay_session(&session.session_id)
        .await
        .expect("replay");
    assert!(
        events
            .events
            .iter()
            .any(|event| event.kind == EventKind::ToolResult)
    );
}

#[tokio::test]
async fn general_subagent_rolls_back_when_verification_fails() {
    let temp = tempfile::tempdir().expect("temp");
    let storage =
        thais_storage::StorageHandle::open(temp.path().join("thais.db")).expect("storage");
    let provider: Arc<dyn thais_providers::Provider> = Arc::new(FakeProvider::with_scripts(
        "fake",
        vec![
            script(vec![FakeStep::ToolCall {
                index: 0,
                id: "call_general".to_owned(),
                name: "subagent".to_owned(),
                arguments: json!({
                    "kind": "general",
                    "task": "create a file"
                })
                .to_string(),
            }]),
            script(vec![FakeStep::ToolCall {
                index: 0,
                id: "call_write".to_owned(),
                name: "write".to_owned(),
                arguments: json!({
                    "path": "generated.txt",
                    "content": "temporary\n"
                })
                .to_string(),
            }]),
            script(vec![FakeStep::Text {
                text: "general failed".to_owned(),
            }]),
        ],
    ));
    let runtime = Arc::new(
        Runtime::new(
            storage,
            provider,
            Arc::new(FixedClock::new("2026-10-23T00:00:00Z")),
            Arc::new(SequenceIdGenerator::new("g")),
            ContextManager::empty(),
        )
        .with_snapshot_base(temp.path().join("snapshots"))
        .with_checkpoint_base(temp.path().join("sessions"))
        .with_verifier(thais_core::Verifier::new(
            vec!["false".to_owned()],
            Duration::from_secs(5),
        )),
    );
    let session = runtime
        .create_session(
            temp.path().to_string_lossy().into_owned(),
            SessionMode::Build,
            None,
        )
        .await
        .expect("create");
    let mut live = runtime.subscribe_live();
    runtime
        .send(&session.session_id, "delegate a build".to_owned(), None)
        .await
        .expect("send");
    let mut approved = 0;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let event = live.recv().await.expect("event");
            if event.kind == EventKind::PermissionRequested {
                approved += 1;
                runtime
                    .approve(
                        &event.session_id,
                        event.payload["approval_id"].as_str().expect("approval id"),
                        ApprovalDecision::Allow,
                    )
                    .await
                    .expect("approve");
            }
            if event.session_id == session.session_id && event.kind == EventKind::TurnCompleted {
                break;
            }
        }
    })
    .await
    .expect("parent turn");
    assert_eq!(approved, 2);
    assert!(!temp.path().join("generated.txt").exists());
}

#[tokio::test]
async fn mcp_tool_calls_are_gated_and_audited() {
    if std::process::Command::new("python3")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_err()
    {
        return;
    }
    let temp = tempfile::tempdir().expect("temp");
    let server_script = temp.path().join("fake_mcp.py");
    std::fs::write(
        &server_script,
        r#"
import json, sys
for line in sys.stdin:
    message = json.loads(line)
    method = message.get("method")
    ident = message.get("id")
    if method == "initialize":
        print(json.dumps({"jsonrpc":"2.0","id":ident,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"fake","version":"1"}}}), flush=True)
    elif method == "tools/list":
        print(json.dumps({"jsonrpc":"2.0","id":ident,"result":{"tools":[{"name":"echo","description":"Echo","inputSchema":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}}]}}), flush=True)
    elif method == "tools/call":
        print(json.dumps({"jsonrpc":"2.0","id":ident,"result":{"content":[{"type":"text","text":"echo: " + message["params"]["arguments"]["text"]}],"isError":False}}), flush=True)
"#,
    )
    .expect("script");
    let storage =
        thais_storage::StorageHandle::open(temp.path().join("thais.db")).expect("storage");
    let provider: Arc<dyn thais_providers::Provider> = Arc::new(FakeProvider::with_scripts(
        "fake",
        vec![
            script(vec![FakeStep::ToolCall {
                index: 0,
                id: "call_mcp".to_owned(),
                name: "mcp__fake__echo".to_owned(),
                arguments: json!({"text": "hello"}).to_string(),
            }]),
            script(vec![FakeStep::Text {
                text: "mcp complete".to_owned(),
            }]),
        ],
    ));
    let config = thais_tools::McpConfig {
        servers: vec![thais_tools::McpServerConfig {
            id: "fake".to_owned(),
            command: "python3".to_owned(),
            url: None,
            args: vec![server_script.to_string_lossy().into_owned()],
            timeout_ms: 2_000,
            read_only: false,
            transport: "stdio".to_owned(),
        }],
    };
    let runtime = Arc::new(
        Runtime::new(
            storage,
            provider,
            Arc::new(FixedClock::new("2026-10-23T00:00:00Z")),
            Arc::new(SequenceIdGenerator::new("m")),
            ContextManager::empty(),
        )
        .with_snapshot_base(temp.path().join("snapshots"))
        .with_checkpoint_base(temp.path().join("sessions"))
        .with_mcp(config),
    );
    let session = runtime
        .create_session(
            temp.path().to_string_lossy().into_owned(),
            SessionMode::Build,
            None,
        )
        .await
        .expect("create");
    let mut live = runtime.subscribe_live();
    runtime
        .send(&session.session_id, "use mcp".to_owned(), None)
        .await
        .expect("send");
    let mut approval_id = None;
    let mut result = None;
    tokio::time::timeout(Duration::from_secs(4), async {
        loop {
            let event = live.recv().await.expect("event");
            if event.kind == EventKind::PermissionRequested {
                approval_id = event.payload["approval_id"].as_str().map(ToOwned::to_owned);
                runtime
                    .approve(
                        &session.session_id,
                        approval_id.as_deref().expect("id"),
                        ApprovalDecision::Allow,
                    )
                    .await
                    .expect("approve");
            }
            if event.session_id == session.session_id
                && event.kind == EventKind::ToolResult
                && event.payload["name"] == "mcp__fake__echo"
            {
                result = Some(event.clone());
            }
            if event.session_id == session.session_id && event.kind == EventKind::TurnCompleted {
                break;
            }
        }
    })
    .await
    .expect("turn");
    assert!(approval_id.is_some());
    assert!(
        result.expect("mcp result").payload["content"]
            .as_str()
            .expect("content")
            .contains("echo: hello")
    );
    let replay = runtime
        .replay_session(&session.session_id)
        .await
        .expect("replay");
    assert!(
        replay
            .events
            .iter()
            .any(|event| event.kind == EventKind::PermissionRequested)
    );
}

#[tokio::test]
async fn lsp_diagnostics_feed_the_verifier_and_roll_back_edits() {
    if std::process::Command::new("python3")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_err()
    {
        return;
    }
    let temp = tempfile::tempdir().expect("temp");
    let lsp_script = temp.path().join("fake_lsp.py");
    std::fs::write(
        &lsp_script,
        r#"
import json, sys
def read():
    headers = {}
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            return None
        if line in (b"\r\n", b"\n"):
            break
        key, value = line.decode().split(":", 1)
        headers[key.lower()] = value.strip()
    return json.loads(sys.stdin.buffer.read(int(headers["content-length"])))
def write(value):
    body = json.dumps(value).encode()
    sys.stdout.buffer.write(("Content-Length: %d\r\n\r\n" % len(body)).encode() + body)
    sys.stdout.buffer.flush()
while True:
    message = read()
    if message is None:
        break
    method = message.get("method")
    ident = message.get("id")
    if method == "initialize":
        write({"jsonrpc":"2.0","id":ident,"result":{"capabilities":{}}})
    elif method == "textDocument/didOpen":
        write({"jsonrpc":"2.0","method":"textDocument/publishDiagnostics","params":{"uri":message["params"]["textDocument"]["uri"],"diagnostics":[{"range":{"start":{"line":0,"character":0}},"severity":1,"message":"fake lsp error"}]}})
"#,
    )
    .expect("lsp script");
    let lsp_config_path = temp.path().join("lsp.toml");
    std::fs::write(
        &lsp_config_path,
        format!(
            r#"
[[servers]]
id = "fake"
command = "python3"
args = ["{}"]
extensions = ["txt"]
timeout_ms = 2000
"#,
            lsp_script.to_string_lossy()
        ),
    )
    .expect("lsp config");
    let storage =
        thais_storage::StorageHandle::open(temp.path().join("thais.db")).expect("storage");
    let provider: Arc<dyn thais_providers::Provider> = Arc::new(FakeProvider::with_scripts(
        "fake",
        vec![
            script(vec![FakeStep::ToolCall {
                index: 0,
                id: "call_write".to_owned(),
                name: "write".to_owned(),
                arguments: json!({"path": "main.txt", "content": "bad\n"}).to_string(),
            }]),
            script(vec![FakeStep::Text {
                text: "verification failed".to_owned(),
            }]),
        ],
    ));
    let runtime = Arc::new(
        Runtime::new(
            storage,
            provider,
            Arc::new(FixedClock::new("2026-10-23T00:00:00Z")),
            Arc::new(SequenceIdGenerator::new("l")),
            ContextManager::empty(),
        )
        .with_snapshot_base(temp.path().join("snapshots"))
        .with_checkpoint_base(temp.path().join("sessions"))
        .with_lsp(thais_core::LspConfig::load(&lsp_config_path).expect("lsp")),
    );
    let session = runtime
        .create_session(
            temp.path().to_string_lossy().into_owned(),
            SessionMode::Build,
            None,
        )
        .await
        .expect("create");
    let mut live = runtime.subscribe_live();
    runtime
        .send(&session.session_id, "write code".to_owned(), None)
        .await
        .expect("send");
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            let event = live.recv().await.expect("event");
            if event.kind == EventKind::PermissionRequested {
                runtime
                    .approve(
                        &event.session_id,
                        event.payload["approval_id"].as_str().expect("approval id"),
                        ApprovalDecision::Allow,
                    )
                    .await
                    .expect("approve");
            }
            if event.session_id == session.session_id && event.kind == EventKind::TurnCompleted {
                break;
            }
        }
    })
    .await
    .expect("turn");
    assert!(!temp.path().join("main.txt").exists());
    let replay = runtime
        .replay_session(&session.session_id)
        .await
        .expect("replay");
    assert!(
        replay
            .events
            .iter()
            .any(|event| event.kind == EventKind::LspDiagnostics)
    );
}

#[tokio::test]
async fn tool_output_is_redacted_before_the_model_and_event_log() {
    let temp = tempfile::tempdir().expect("temp");
    let runtime = runtime_with_scripts(
        &temp,
        vec![
            script(vec![FakeStep::ToolCall {
                index: 0,
                id: "call_secret".to_owned(),
                name: "bash".to_owned(),
                arguments: json!({"command": "printf sk-live-secret-123"}).to_string(),
            }]),
            script(vec![FakeStep::Text {
                text: "done".to_owned(),
            }]),
        ],
    );
    let session = runtime
        .create_session(
            temp.path().to_string_lossy().into_owned(),
            SessionMode::Build,
            None,
        )
        .await
        .expect("create");
    let mut live = runtime.subscribe_live();
    runtime
        .send(&session.session_id, "run".to_owned(), None)
        .await
        .expect("send");
    tokio::time::timeout(Duration::from_secs(4), async {
        loop {
            let event = live.recv().await.expect("event");
            if event.kind == EventKind::PermissionRequested {
                runtime
                    .approve(
                        &event.session_id,
                        event.payload["approval_id"].as_str().expect("approval id"),
                        ApprovalDecision::Allow,
                    )
                    .await
                    .expect("approve");
            }
            if event.session_id == session.session_id && event.kind == EventKind::TurnCompleted {
                break;
            }
        }
    })
    .await
    .expect("turn");
    let replay = runtime
        .replay_session(&session.session_id)
        .await
        .expect("replay");
    let encoded = serde_json::to_string(&replay.events).expect("json");
    assert!(!encoded.contains("sk-live-secret-123"), "{encoded}");
    assert!(encoded.contains("[REDACTED]"));
}

#[tokio::test]
async fn crashed_lease_can_be_released_before_recovery() {
    let temp = tempfile::tempdir().expect("temp");
    let first = runtime(&temp, script(vec![]));
    let workspace = temp.path().to_string_lossy().into_owned();
    let session = first
        .create_session(workspace.clone(), SessionMode::Build, None)
        .await
        .expect("first");
    // Simulate a crashed owner by expiring its lease, then let another
    // runtime recover the workspace.
    let storage =
        thais_storage::StorageHandle::open(temp.path().join("thais.db")).expect("storage");
    storage
        .release_lease(session.workspace.clone(), session.session_id.clone())
        .await
        .expect("release crashed owner");
    let second = runtime_with_prefix(&temp, script(vec![]), "z");
    second
        .create_session(workspace, SessionMode::Build, None)
        .await
        .expect("recovery");
}
