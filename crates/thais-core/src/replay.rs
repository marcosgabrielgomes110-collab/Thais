//! Rebuild a session view from the append-only Event Log.

use thais_protocol::event::{EventEnvelope, EventKind};
use thais_storage::{StorageError, StorageHandle};
use thiserror::Error;

/// A reconstructed view of one session.
#[derive(Debug, Clone)]
pub struct ReplayState {
    /// Events in sequence order.
    pub events: Vec<EventEnvelope>,
    /// Concatenated model text.
    pub transcript: String,
    /// Highest observed sequence number.
    pub last_seq: u64,
}

/// Replay failures.
#[derive(Debug, Error)]
pub enum ReplayError {
    /// Storage failure.
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),
}

/// Replay a session from its event log.
pub async fn replay_session(
    storage: &StorageHandle,
    session_id: &str,
) -> Result<ReplayState, ReplayError> {
    let events = storage.events_after(session_id.to_owned(), 0).await?;
    let mut transcript = String::new();
    for event in &events {
        if event.kind == EventKind::ModelDelta
            && let Some(text) = event
                .payload
                .get("text")
                .and_then(serde_json::Value::as_str)
        {
            transcript.push_str(text);
        }
    }
    let last_seq = events.last().map_or(0, |event| event.seq);
    Ok(ReplayState {
        events,
        transcript,
        last_seq,
    })
}
