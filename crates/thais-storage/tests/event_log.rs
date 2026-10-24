use std::path::Path;

use rusqlite::Connection;
use serde_json::json;
use tempfile::TempDir;
use thais_protocol::event::EventKind;
use thais_protocol::session::{SessionInfo, SessionMode, SessionStatus};
use thais_storage::storage::{NewEvent, Storage, StorageError, StorageHandle};
use thais_storage::trust::trust_key;

fn session(workspace: &Path) -> SessionInfo {
    let now = "2026-10-23T00:00:00Z".to_owned();
    SessionInfo {
        session_id: "s-1".to_owned(),
        parent_id: None,
        name: "s-1".to_owned(),
        workspace: workspace.to_string_lossy().into_owned(),
        mode: SessionMode::Build,
        model: "fake".to_owned(),
        model_policy: serde_json::json!({"default": "fake"}),
        input_tokens: 0,
        output_tokens: 0,
        cost_usd: 0.0,
        status: SessionStatus::Active,
        created_at: now.clone(),
        updated_at: now,
    }
}

fn event(id: &str, text: &str) -> NewEvent {
    NewEvent::new(
        id,
        "s-1",
        "2026-10-23T00:00:01Z",
        EventKind::ModelDelta,
        json!({"text": text}),
    )
}

fn open(temp: &TempDir) -> Storage {
    Storage::open(temp.path().join("thais.db")).expect("storage")
}

#[test]
fn session_crud() {
    let temp = tempfile::tempdir().expect("temp");
    let storage = open(&temp);
    let info = session(temp.path());

    storage.create_session(&info).expect("create");
    assert!(storage.session_exists("s-1").expect("exists"));
    assert_eq!(storage.get_session("s-1").expect("get"), info);
    assert_eq!(storage.list_sessions().expect("list"), vec![info.clone()]);
    assert!(matches!(
        storage.create_session(&info),
        Err(StorageError::SessionAlreadyExists(_))
    ));
    assert!(matches!(
        storage.get_session("missing"),
        Err(StorageError::SessionNotFound(_))
    ));
}

#[test]
fn appends_assign_monotonic_sequences() {
    let temp = tempfile::tempdir().expect("temp");
    let mut storage = open(&temp);
    storage
        .create_session(&session(temp.path()))
        .expect("create");

    let first = storage.append_event(event("e-1", "hello")).expect("first");
    let second = storage.append_event(event("e-2", "world")).expect("second");
    assert_eq!(first.seq, 1);
    assert_eq!(second.seq, 2);
    assert_eq!(storage.last_seq("s-1").expect("last"), 2);

    let all = storage.events_after("s-1", 0).expect("events");
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].event_id, "e-1");
    assert_eq!(all[1].event_id, "e-2");

    let tail = storage.events_after("s-1", 1).expect("tail");
    assert_eq!(tail.len(), 1);
    assert_eq!(tail[0].seq, 2);
}

#[test]
fn missing_session_rejects_event() {
    let temp = tempfile::tempdir().expect("temp");
    let mut storage = open(&temp);
    let result = storage.append_event(event("e-1", "hello"));
    assert!(matches!(result, Err(StorageError::SessionNotFound(_))));
}

#[test]
fn event_table_is_append_only() {
    let temp = tempfile::tempdir().expect("temp");
    let database = temp.path().join("thais.db");
    let mut storage = Storage::open(&database).expect("storage");
    storage
        .create_session(&session(temp.path()))
        .expect("create");
    storage.append_event(event("e-1", "hello")).expect("append");

    let raw = Connection::open(&database).expect("raw connection");
    let update = raw.execute(
        "UPDATE events SET kind = 'error' WHERE event_id = 'e-1'",
        [],
    );
    assert!(update.is_err(), "UPDATE must be rejected: {update:?}");
    let delete = raw.execute("DELETE FROM events WHERE event_id = 'e-1'", []);
    assert!(delete.is_err(), "DELETE must be rejected: {delete:?}");
    assert_eq!(storage.events_after("s-1", 0).expect("events").len(), 1);
}

#[test]
fn fts_index_searches_and_rebuilds() {
    let temp = tempfile::tempdir().expect("temp");
    let database = temp.path().join("thais.db");
    let mut storage = Storage::open(&database).expect("storage");
    storage
        .create_session(&session(temp.path()))
        .expect("create");
    storage
        .append_event(event("e-1", "alpha beta gamma"))
        .expect("append");

    assert!(storage.has_fts5().expect("fts5"));
    assert_eq!(storage.search("alpha", 10).expect("search").len(), 1);

    let raw = Connection::open(&database).expect("raw connection");
    raw.execute("DELETE FROM events_fts", [])
        .expect("clear fts");
    assert!(storage.search("alpha", 10).expect("search").is_empty());

    storage.rebuild_fts().expect("rebuild");
    assert_eq!(storage.search("alpha", 10).expect("search").len(), 1);
}

#[test]
fn redaction_applies_before_persistence() {
    let temp = tempfile::tempdir().expect("temp");
    let database = temp.path().join("thais.db");
    let mut storage = Storage::open(&database).expect("storage");
    storage
        .create_session(&session(temp.path()))
        .expect("create");

    let appended = storage
        .append_event(NewEvent::new(
            "e-1",
            "s-1",
            "2026-10-23T00:00:01Z",
            EventKind::ModelDelta,
            json!({"api_key": "sk-live-123", "text": "visible"}),
        ))
        .expect("append");

    assert_eq!(appended.payload["api_key"], "[REDACTED]");
    assert_eq!(appended.payload["text"], "visible");

    let raw = Connection::open(&database).expect("raw connection");
    let payload: String = raw
        .query_row(
            "SELECT payload_json FROM events WHERE event_id = 'e-1'",
            [],
            |row| row.get(0),
        )
        .expect("payload");
    assert!(!payload.contains("sk-live-123"));
    assert!(payload.contains("[REDACTED]"));
}

#[test]
fn trust_is_keyed_by_path_and_config_hash() {
    let temp = tempfile::tempdir().expect("temp");
    let workspace = temp.path().join("repo");
    std::fs::create_dir_all(workspace.join(".thais")).expect("config dir");
    let config = workspace.join(".thais/config.toml");
    std::fs::write(&config, "theme = \"dark\"\n").expect("config");

    let storage = open(&temp);
    let (path, hash) = trust_key(&workspace).expect("trust key");
    assert!(!storage.is_trusted(&path, &hash).expect("trusted"));

    storage.trust_workspace(&path, &hash).expect("trust");
    assert!(storage.is_trusted(&path, &hash).expect("trusted"));
    assert_eq!(storage.trust_records().expect("records").len(), 1);

    std::fs::write(&config, "theme = \"light\"\n").expect("config");
    let (_, changed_hash) = trust_key(&workspace).expect("trust key");
    assert!(
        !storage
            .is_trusted(&path, &changed_hash)
            .expect("trusted after change")
    );

    assert_eq!(storage.remove_trust(&path).expect("remove"), 1);
    assert!(
        !storage
            .is_trusted(&path, &hash)
            .expect("trusted after remove")
    );
}

#[test]
fn schema_version_and_integrity() {
    let temp = tempfile::tempdir().expect("temp");
    let storage = open(&temp);
    assert_eq!(storage.schema_version().expect("version"), 3);
    assert_eq!(storage.integrity_check().expect("integrity"), "ok");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_appends_get_unique_sequences() {
    let temp = tempfile::tempdir().expect("temp");
    let handle = StorageHandle::open(temp.path().join("thais.db")).expect("handle");
    handle
        .create_session(session(temp.path()))
        .await
        .expect("create");

    let mut tasks = Vec::new();
    for index in 0..10 {
        let handle = handle.clone();
        tasks.push(tokio::spawn(async move {
            handle
                .append_event(NewEvent::new(
                    format!("e-{index}"),
                    "s-1",
                    "2026-10-23T00:00:01Z",
                    EventKind::ModelDelta,
                    json!({"text": format!("event {index}")}),
                ))
                .await
                .expect("append")
                .seq
        }));
    }

    let mut sequences = Vec::new();
    for task in tasks {
        sequences.push(task.await.expect("join"));
    }
    sequences.sort_unstable();
    assert_eq!(sequences, (1..=10).collect::<Vec<_>>());
}
