//! Runtime-owned session checkpoints.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thais_protocol::session::{CheckpointSummary, SessionMode};

const OPEN: &str = "<!-- thais-checkpoint";
const CLOSE: &str = "-->";

/// Structured checkpoint state for one session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointState {
    /// Session identifier.
    pub session_id: String,
    /// Human-readable name.
    pub name: String,
    /// Workspace root.
    pub workspace: String,
    /// Active mode.
    pub mode: SessionMode,
    /// Role-based model policy.
    pub model_policy: serde_json::Value,
    /// Current loop state.
    pub state: String,
    /// Approved plan, when present.
    pub plan: Option<String>,
    /// Task list.
    pub tasks: Vec<String>,
    /// RFC 3339 update time.
    pub updated_at: String,
}

impl CheckpointState {
    /// Human-facing summary.
    #[must_use]
    pub fn summary(&self) -> CheckpointSummary {
        CheckpointSummary {
            state: self.state.clone(),
            plan: self.plan.clone(),
            tasks: self.tasks.clone(),
        }
    }
}

/// Write a checkpoint as Markdown with a machine-readable JSON block.
pub fn write_checkpoint(path: &Path, state: &CheckpointState) -> Result<(), CheckpointError> {
    let json = serde_json::to_string_pretty(state)?;
    let content = format!(
        "# Thais checkpoint\n\n<!-- thais-checkpoint\n{json}\n-->\n\n## State\n\n{}\n",
        state.state
    );
    thais_storage::write_private(path, content.as_bytes()).map_err(CheckpointError::Path)
}

/// Read a checkpoint if it exists and is valid.
pub fn read_checkpoint(path: &Path) -> Result<Option<CheckpointState>, CheckpointError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(CheckpointError::Io {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    let Some(start) = text.find(OPEN) else {
        return Ok(None);
    };
    let after = &text[start + OPEN.len()..];
    let Some(end) = after.find(CLOSE) else {
        return Ok(None);
    };
    let json = after[..end].trim();
    Ok(Some(serde_json::from_str(json)?))
}

/// Checkpoint errors.
#[derive(Debug, thiserror::Error)]
pub enum CheckpointError {
    /// I/O failure.
    #[error("checkpoint I/O error at {path}: {source}")]
    Io {
        /// Path involved.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },
    /// Path/write failure.
    #[error("checkpoint path error: {0}")]
    Path(#[from] thais_storage::PathError),
    /// JSON failure.
    #[error("checkpoint JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn checkpoint_round_trips() {
        let temp = tempfile::tempdir().expect("temp");
        let path = temp.path().join("sessions/s-1/checkpoint.md");
        let state = CheckpointState {
            session_id: "s-1".to_owned(),
            name: "demo".to_owned(),
            workspace: "/tmp/demo".to_owned(),
            mode: SessionMode::Build,
            model_policy: json!({"default": "model"}),
            state: "active".to_owned(),
            plan: Some("plan".to_owned()),
            tasks: vec!["one".to_owned()],
            updated_at: "2026-10-23T00:00:00Z".to_owned(),
        };
        write_checkpoint(&path, &state).expect("write");
        let loaded = read_checkpoint(&path).expect("read").expect("state");
        assert_eq!(loaded.session_id, "s-1");
        assert_eq!(loaded.mode, SessionMode::Build);
        assert_eq!(loaded.tasks, ["one"]);
    }
}
