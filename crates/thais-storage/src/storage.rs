//! SQLite-backed sessions, append-only Event Log, FTS5 index, and workspace trust.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, params};
use thais_protocol::error::ProtocolError;
use thais_protocol::event::{EventEnvelope, EventKind};
use thais_protocol::session::{ApprovalInfo, SessionInfo, SessionMode, SessionStatus};
use thiserror::Error;

use crate::redaction::{BasicRedactor, Redactor};
use crate::trust::{self, TrustRecord};

/// One event to append. The store assigns the per-session sequence number.
#[derive(Debug, Clone)]
pub struct NewEvent {
    /// Stable event identifier.
    pub event_id: String,
    /// Session this event belongs to.
    pub session_id: String,
    /// RFC 3339 timestamp.
    pub timestamp: String,
    /// Event kind.
    pub kind: EventKind,
    /// Event-specific payload.
    pub payload: serde_json::Value,
}

impl NewEvent {
    /// Build a new event.
    #[must_use]
    pub fn new(
        event_id: impl Into<String>,
        session_id: impl Into<String>,
        timestamp: impl Into<String>,
        kind: EventKind,
        payload: serde_json::Value,
    ) -> Self {
        Self {
            event_id: event_id.into(),
            session_id: session_id.into(),
            timestamp: timestamp.into(),
            kind,
            payload,
        }
    }
}

/// One FTS match with its session label.
#[derive(Debug, Clone)]
pub struct HistoryHit {
    /// Matching event.
    pub event: EventEnvelope,
    /// Session name.
    pub session_name: String,
    /// Canonical workspace.
    pub workspace: String,
}

/// A mutating workspace lease.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceLease {
    /// Canonical workspace path.
    pub canonical_workspace: String,
    /// Session holding the lease.
    pub session_id: String,
    /// RFC 3339 acquisition time.
    pub acquired_at: String,
    /// RFC 3339 heartbeat time.
    pub heartbeat_at: String,
    /// RFC 3339 expiry time.
    pub expires_at: String,
}

/// Errors from the storage layer.
#[derive(Debug, Error)]
pub enum StorageError {
    /// SQLite failure.
    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// JSON serialization failure.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    /// Protocol type parsing failure.
    #[error("protocol error: {0}")]
    Protocol(#[from] ProtocolError),
    /// I/O failure.
    #[error("I/O error at {path}: {source}")]
    Io {
        /// Path involved in the failure.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },
    /// The requested session does not exist.
    #[error("session not found: {0}")]
    SessionNotFound(String),
    /// The session identifier is already in use.
    #[error("session already exists: {0}")]
    SessionAlreadyExists(String),
    /// The workspace path is not valid UTF-8.
    #[error("workspace path is not valid UTF-8: {0}")]
    InvalidPath(PathBuf),
    /// A sequence number did not fit in the expected range.
    #[error("invalid event sequence: {0}")]
    InvalidSequence(i64),
    /// The database schema is newer than this binary.
    #[error("unsupported schema version: {0}")]
    UnsupportedSchema(i64),
    /// The storage lock was poisoned.
    #[error("storage lock poisoned")]
    Poisoned,
    /// A blocking storage task failed.
    #[error("storage task failed: {0}")]
    Join(String),
}

/// Synchronous storage handle backed by one SQLite connection.
pub struct Storage {
    conn: Connection,
    redactor: Box<dyn Redactor>,
}

impl Storage {
    /// Open or create a database with the default redactor.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StorageError> {
        Self::open_with_redactor(path, Box::new(BasicRedactor))
    }

    /// Open or create a database with a custom redactor.
    pub fn open_with_redactor(
        path: impl AsRef<Path>,
        redactor: Box<dyn Redactor>,
    ) -> Result<Self, StorageError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| StorageError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }

        let conn = Connection::open(path)?;
        let _mode: String = conn.query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))?;
        conn.execute_batch(
            "PRAGMA foreign_keys=ON;
             PRAGMA busy_timeout=5000;
             PRAGMA synchronous=NORMAL;",
        )?;

        let mut storage = Self { conn, redactor };
        storage.apply_migrations()?;
        Ok(storage)
    }

    fn apply_migrations(&mut self) -> Result<(), StorageError> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_migrations (
                version INTEGER PRIMARY KEY,
                applied_at TEXT NOT NULL
            );",
        )?;

        let mut current: i64 = self.conn.query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
            [],
            |row| row.get(0),
        )?;

        if current > 3 {
            return Err(StorageError::UnsupportedSchema(current));
        }
        if current == 0 {
            self.conn.execute_batch(MIGRATION_1)?;
            self.conn.execute(
                "INSERT INTO schema_migrations(version, applied_at) VALUES (1, ?1)",
                params![Utc::now().to_rfc3339()],
            )?;
            current = 1;
        }
        if current == 1 {
            self.conn.execute_batch(MIGRATION_2)?;
            self.conn.execute(
                "INSERT INTO schema_migrations(version, applied_at) VALUES (2, ?1)",
                params![Utc::now().to_rfc3339()],
            )?;
            current = 2;
        }
        if current == 2 {
            self.conn.execute_batch(MIGRATION_3)?;
            self.conn.execute(
                "INSERT INTO schema_migrations(version, applied_at) VALUES (3, ?1)",
                params![Utc::now().to_rfc3339()],
            )?;
        }
        Ok(())
    }

    /// Insert a session record.
    pub fn create_session(&self, session: &SessionInfo) -> Result<(), StorageError> {
        if self.session_exists(&session.session_id)? {
            return Err(StorageError::SessionAlreadyExists(
                session.session_id.clone(),
            ));
        }

        let canonical = trust::canonical_workspace(Path::new(&session.workspace))?;
        let canonical = canonical
            .to_str()
            .ok_or_else(|| StorageError::InvalidPath(canonical.clone()))?
            .to_owned();

        self.conn.execute(
            "INSERT INTO sessions (
                session_id, parent_id, name, workspace, canonical_workspace, mode, model,
                model_policy_json, status, created_at, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                session.session_id,
                session.parent_id,
                session.name,
                session.workspace,
                canonical,
                session.mode.as_str(),
                session.model,
                serde_json::to_string(&session.model_policy)?,
                session.status.as_str(),
                session.created_at,
                session.updated_at,
            ],
        )?;
        Ok(())
    }

    /// Fetch one session.
    pub fn get_session(&self, session_id: &str) -> Result<SessionInfo, StorageError> {
        let row = self
            .conn
            .query_row(
                "SELECT session_id, parent_id, name, workspace, mode, model, model_policy_json,
                        input_tokens, output_tokens, cost_usd, status, created_at, updated_at
                 FROM sessions WHERE session_id = ?1",
                params![session_id],
                SessionRow::from_row,
            )
            .optional()?;

        match row {
            Some(row) => row.into_session(),
            None => Err(StorageError::SessionNotFound(session_id.to_owned())),
        }
    }

    /// List sessions in creation order.
    pub fn list_sessions(&self) -> Result<Vec<SessionInfo>, StorageError> {
        self.list_sessions_with_archived(false)
    }

    /// List sessions, optionally including archived ones. Soft-deleted sessions
    /// are never listed.
    pub fn list_sessions_with_archived(
        &self,
        include_archived: bool,
    ) -> Result<Vec<SessionInfo>, StorageError> {
        let sql = if include_archived {
            "SELECT session_id, parent_id, name, workspace, mode, model, model_policy_json,
                    input_tokens, output_tokens, cost_usd, status, created_at, updated_at
             FROM sessions WHERE status != 'deleted'
             ORDER BY created_at ASC, session_id ASC"
        } else {
            "SELECT session_id, parent_id, name, workspace, mode, model, model_policy_json,
                    input_tokens, output_tokens, cost_usd, status, created_at, updated_at
             FROM sessions WHERE status != 'deleted' AND status != 'archived'
             ORDER BY created_at ASC, session_id ASC"
        };
        let mut statement = self.conn.prepare(sql)?;
        let rows = statement.query_map([], SessionRow::from_row)?;
        let rows = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter().map(SessionRow::into_session).collect()
    }

    /// Whether a session exists.
    pub fn session_exists(&self, session_id: &str) -> Result<bool, StorageError> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM sessions WHERE session_id = ?1",
            params![session_id],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    /// Update the active mode of a session.
    pub fn update_session_mode(
        &self,
        session_id: &str,
        mode: SessionMode,
    ) -> Result<(), StorageError> {
        let updated = self.conn.execute(
            "UPDATE sessions SET mode = ?1, updated_at = ?2 WHERE session_id = ?3",
            params![mode.as_str(), Utc::now().to_rfc3339(), session_id],
        )?;
        if updated == 0 {
            return Err(StorageError::SessionNotFound(session_id.to_owned()));
        }
        Ok(())
    }

    /// Rename a session.
    pub fn rename_session(
        &self,
        session_id: &str,
        name: &str,
    ) -> Result<SessionInfo, StorageError> {
        let updated = self.conn.execute(
            "UPDATE sessions SET name = ?1, updated_at = ?2 WHERE session_id = ?3",
            params![name, Utc::now().to_rfc3339(), session_id],
        )?;
        if updated == 0 {
            return Err(StorageError::SessionNotFound(session_id.to_owned()));
        }
        self.get_session(session_id)
    }

    /// Archive a session.
    pub fn archive_session(&self, session_id: &str) -> Result<SessionInfo, StorageError> {
        let now = Utc::now().to_rfc3339();
        let updated = self.conn.execute(
            "UPDATE sessions SET status = 'archived', archived_at = ?1, updated_at = ?1
             WHERE session_id = ?2",
            params![now, session_id],
        )?;
        if updated == 0 {
            return Err(StorageError::SessionNotFound(session_id.to_owned()));
        }
        self.get_session(session_id)
    }

    /// Soft-delete a session while preserving its Event Log.
    pub fn delete_session(&self, session_id: &str) -> Result<bool, StorageError> {
        let now = Utc::now().to_rfc3339();
        let updated = self.conn.execute(
            "UPDATE sessions SET status = 'deleted', deleted_at = ?1, updated_at = ?1
             WHERE session_id = ?2",
            params![now, session_id],
        )?;
        Ok(updated > 0)
    }

    /// Update the role-based model policy.
    pub fn update_session_model_policy(
        &self,
        session_id: &str,
        policy: &serde_json::Value,
    ) -> Result<SessionInfo, StorageError> {
        let updated = self.conn.execute(
            "UPDATE sessions SET model_policy_json = ?1, updated_at = ?2 WHERE session_id = ?3",
            params![
                serde_json::to_string(policy)?,
                Utc::now().to_rfc3339(),
                session_id
            ],
        )?;
        if updated == 0 {
            return Err(StorageError::SessionNotFound(session_id.to_owned()));
        }
        self.get_session(session_id)
    }

    /// Add a turn's usage and cost to the session totals.
    pub fn record_session_usage(
        &self,
        session_id: &str,
        input_tokens: u64,
        output_tokens: u64,
        cost_usd: f64,
    ) -> Result<SessionInfo, StorageError> {
        let updated = self.conn.execute(
            "UPDATE sessions SET
                input_tokens = input_tokens + ?1,
                output_tokens = output_tokens + ?2,
                cost_usd = cost_usd + ?3,
                updated_at = ?4
             WHERE session_id = ?5",
            params![
                input_tokens as i64,
                output_tokens as i64,
                cost_usd,
                Utc::now().to_rfc3339(),
                session_id
            ],
        )?;
        if updated == 0 {
            return Err(StorageError::SessionNotFound(session_id.to_owned()));
        }
        self.get_session(session_id)
    }

    /// Update the session's default model column.
    pub fn update_session_model(
        &self,
        session_id: &str,
        model: &str,
    ) -> Result<SessionInfo, StorageError> {
        let updated = self.conn.execute(
            "UPDATE sessions SET model = ?1, updated_at = ?2 WHERE session_id = ?3",
            params![model, Utc::now().to_rfc3339(), session_id],
        )?;
        if updated == 0 {
            return Err(StorageError::SessionNotFound(session_id.to_owned()));
        }
        self.get_session(session_id)
    }

    /// Acquire or refresh the mutating lease for a workspace.
    pub fn acquire_lease(
        &mut self,
        canonical_workspace: &str,
        session_id: &str,
        ttl_secs: u64,
        steal: bool,
    ) -> Result<Option<WorkspaceLease>, StorageError> {
        let now = Utc::now();
        let now_text = now.to_rfc3339();
        let expires = (now + chrono::Duration::seconds(ttl_secs as i64)).to_rfc3339();
        let existing = self.lease_holder(canonical_workspace)?;

        if let Some(lease) = &existing {
            let expired = lease.expires_at.as_str() <= now_text.as_str();
            if lease.session_id != session_id && !expired && !steal {
                return Ok(Some(lease.clone()));
            }
        }

        self.conn.execute(
            "INSERT INTO workspace_leases (
                canonical_workspace, session_id, acquired_at, heartbeat_at, expires_at
             ) VALUES (?1, ?2, ?3, ?3, ?4)
             ON CONFLICT(canonical_workspace) DO UPDATE SET
                session_id = excluded.session_id,
                acquired_at = excluded.acquired_at,
                heartbeat_at = excluded.heartbeat_at,
                expires_at = excluded.expires_at",
            params![canonical_workspace, session_id, now_text, expires],
        )?;
        Ok(None)
    }

    /// Refresh a lease held by a session.
    pub fn renew_lease(
        &self,
        canonical_workspace: &str,
        session_id: &str,
        ttl_secs: u64,
    ) -> Result<bool, StorageError> {
        let now = Utc::now();
        let expires = (now + chrono::Duration::seconds(ttl_secs as i64)).to_rfc3339();
        let updated = self.conn.execute(
            "UPDATE workspace_leases SET heartbeat_at = ?1, expires_at = ?2
             WHERE canonical_workspace = ?3 AND session_id = ?4",
            params![now.to_rfc3339(), expires, canonical_workspace, session_id],
        )?;
        Ok(updated > 0)
    }

    /// Release a lease held by a session.
    pub fn release_lease(
        &self,
        canonical_workspace: &str,
        session_id: &str,
    ) -> Result<bool, StorageError> {
        let deleted = self.conn.execute(
            "DELETE FROM workspace_leases WHERE canonical_workspace = ?1 AND session_id = ?2",
            params![canonical_workspace, session_id],
        )?;
        Ok(deleted > 0)
    }

    /// Current lease holder for a workspace.
    pub fn lease_holder(
        &self,
        canonical_workspace: &str,
    ) -> Result<Option<WorkspaceLease>, StorageError> {
        self.conn
            .query_row(
                "SELECT canonical_workspace, session_id, acquired_at, heartbeat_at, expires_at
                 FROM workspace_leases WHERE canonical_workspace = ?1",
                params![canonical_workspace],
                |row| {
                    Ok(WorkspaceLease {
                        canonical_workspace: row.get(0)?,
                        session_id: row.get(1)?,
                        acquired_at: row.get(2)?,
                        heartbeat_at: row.get(3)?,
                        expires_at: row.get(4)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    /// Persist a pending approval.
    pub fn insert_pending_approval(
        &self,
        approval_id: &str,
        session_id: &str,
        action: &serde_json::Value,
    ) -> Result<(), StorageError> {
        self.conn.execute(
            "INSERT INTO approvals (
                approval_id, session_id, action_hash, status, action_json, created_at
             ) VALUES (?1, ?2, '', 'pending', ?3, ?4)",
            params![
                approval_id,
                session_id,
                serde_json::to_string(action)?,
                Utc::now().to_rfc3339()
            ],
        )?;
        Ok(())
    }

    /// Resolve a pending approval.
    pub fn resolve_approval(
        &self,
        approval_id: &str,
        decision: &str,
        reason: Option<&str>,
    ) -> Result<bool, StorageError> {
        let status = if decision == "allow" {
            "allowed"
        } else {
            "denied"
        };
        let updated = self.conn.execute(
            "UPDATE approvals SET status = ?1, decision = ?2, resolved_at = ?3
             WHERE approval_id = ?4 AND status = 'pending'",
            params![status, decision, Utc::now().to_rfc3339(), approval_id],
        )?;
        let _ = reason;
        Ok(updated > 0)
    }

    /// List pending approvals for a session.
    pub fn pending_approvals(&self, session_id: &str) -> Result<Vec<ApprovalInfo>, StorageError> {
        let mut statement = self.conn.prepare(
            "SELECT approval_id, session_id, action_json, created_at
             FROM approvals WHERE session_id = ?1 AND status = 'pending'
             ORDER BY created_at ASC",
        )?;
        let rows = statement.query_map(params![session_id], |row| {
            let action_json: String = row.get(2)?;
            Ok(ApprovalInfo {
                approval_id: row.get(0)?,
                session_id: row.get(1)?,
                action: serde_json::from_str(&action_json).unwrap_or(serde_json::Value::Null),
                created_at: row.get(3)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Append one event and return it with its assigned sequence number.
    pub fn append_event(&mut self, event: NewEvent) -> Result<EventEnvelope, StorageError> {
        let NewEvent {
            event_id,
            session_id,
            timestamp,
            kind,
            mut payload,
        } = event;

        self.redactor.redact_value(&mut payload);
        let mut search_text = collect_search_text(&payload);
        self.redactor.redact_text(&mut search_text);
        let payload_json = serde_json::to_string(&payload)?;

        let transaction = self.conn.transaction()?;
        let exists: i64 = transaction.query_row(
            "SELECT COUNT(*) FROM sessions WHERE session_id = ?1",
            params![session_id],
            |row| row.get(0),
        )?;
        if exists == 0 {
            return Err(StorageError::SessionNotFound(session_id));
        }

        let next_seq: i64 = transaction.query_row(
            "SELECT COALESCE(MAX(seq), 0) + 1 FROM events WHERE session_id = ?1",
            params![session_id],
            |row| row.get(0),
        )?;
        let seq = u64::try_from(next_seq).map_err(|_| StorageError::InvalidSequence(next_seq))?;

        transaction.execute(
            "INSERT INTO events (
                event_id, session_id, seq, timestamp, kind, payload_json, search_text
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                event_id,
                session_id,
                next_seq,
                timestamp,
                kind.as_str(),
                payload_json,
                search_text,
            ],
        )?;
        transaction.commit()?;

        Ok(EventEnvelope {
            event_id,
            session_id,
            seq,
            timestamp,
            kind,
            payload,
        })
    }

    /// Read events after a sequence number, in order.
    pub fn events_after(
        &self,
        session_id: &str,
        after_seq: u64,
    ) -> Result<Vec<EventEnvelope>, StorageError> {
        let after_seq =
            i64::try_from(after_seq).map_err(|_| StorageError::InvalidSequence(i64::MAX))?;
        let mut statement = self.conn.prepare(
            "SELECT event_id, session_id, seq, timestamp, kind, payload_json
             FROM events WHERE session_id = ?1 AND seq > ?2 ORDER BY seq ASC",
        )?;
        let rows = statement.query_map(params![session_id, after_seq], EventRow::from_row)?;
        let rows = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter().map(EventRow::into_event).collect()
    }

    /// Last sequence number for a session, or zero when it has no events.
    pub fn last_seq(&self, session_id: &str) -> Result<u64, StorageError> {
        let seq: i64 = self.conn.query_row(
            "SELECT COALESCE(MAX(seq), 0) FROM events WHERE session_id = ?1",
            params![session_id],
            |row| row.get(0),
        )?;
        u64::try_from(seq).map_err(|_| StorageError::InvalidSequence(seq))
    }

    /// Search the derived FTS5 index.
    pub fn search(&self, query: &str, limit: u32) -> Result<Vec<EventEnvelope>, StorageError> {
        if query.trim().is_empty() {
            return Ok(Vec::new());
        }
        let limit = i64::from(limit);
        let mut statement = self.conn.prepare(
            "SELECT e.event_id, e.session_id, e.seq, e.timestamp, e.kind, e.payload_json
             FROM events e
             WHERE e.event_id IN (
                 SELECT event_id FROM events_fts WHERE events_fts MATCH ?1
             )
             ORDER BY e.seq ASC LIMIT ?2",
        )?;
        let rows = statement.query_map(params![query, limit], EventRow::from_row)?;
        let rows = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter().map(EventRow::into_event).collect()
    }

    /// Search with optional session and workspace filters.
    pub fn search_filtered(
        &self,
        query: &str,
        session_id: Option<&str>,
        workspace: Option<&str>,
        limit: u32,
    ) -> Result<Vec<HistoryHit>, StorageError> {
        if query.trim().is_empty() {
            return Ok(Vec::new());
        }
        let limit = i64::from(limit.clamp(1, 200));
        let mut statement = self.conn.prepare(
            "SELECT e.event_id, e.session_id, e.seq, e.timestamp, e.kind, e.payload_json,
                    s.name, s.canonical_workspace
             FROM events e
             JOIN sessions s ON s.session_id = e.session_id
             WHERE e.event_id IN (
                 SELECT event_id FROM events_fts WHERE events_fts MATCH ?1
             )
             AND (?2 IS NULL OR e.session_id = ?2)
             AND (?3 IS NULL OR s.canonical_workspace = ?3 OR s.workspace = ?3)
             ORDER BY e.seq ASC LIMIT ?4",
        )?;
        let rows = statement.query_map(params![query, session_id, workspace, limit], |row| {
            Ok((
                EventRow::from_row(row)?,
                row.get::<_, String>(6)?,
                row.get::<_, String>(7)?,
            ))
        })?;
        let rows = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter()
            .map(|(event, session_name, workspace)| {
                Ok(HistoryHit {
                    event: event.into_event()?,
                    session_name,
                    workspace,
                })
            })
            .collect()
    }

    /// Rebuild the derived FTS5 index from the append-only table.
    pub fn rebuild_fts(&self) -> Result<(), StorageError> {
        self.conn.execute_batch(
            "DELETE FROM events_fts;
             INSERT INTO events_fts(event_id, session_id, kind, search_text)
             SELECT event_id, session_id, kind, search_text FROM events;",
        )?;
        Ok(())
    }

    /// Whether this SQLite build includes FTS5.
    pub fn has_fts5(&self) -> Result<bool, StorageError> {
        let enabled: i64 = self.conn.query_row(
            "SELECT sqlite_compileoption_used('ENABLE_FTS5')",
            [],
            |row| row.get(0),
        )?;
        Ok(enabled != 0)
    }

    /// Current migration version.
    pub fn schema_version(&self) -> Result<i64, StorageError> {
        self.conn
            .query_row(
                "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
                [],
                |row| row.get(0),
            )
            .map_err(Into::into)
    }

    /// Run SQLite's integrity check and return its report.
    pub fn integrity_check(&self) -> Result<String, StorageError> {
        self.conn
            .query_row("PRAGMA integrity_check", [], |row| row.get(0))
            .map_err(Into::into)
    }

    /// Record or refresh a workspace trust decision.
    pub fn trust_workspace(
        &self,
        canonical_path: &str,
        config_hash: &str,
    ) -> Result<(), StorageError> {
        let now = Utc::now().to_rfc3339();
        self.conn.execute(
            "INSERT INTO workspace_trust (
                canonical_path, config_hash, created_at, last_used_at
             ) VALUES (?1, ?2, ?3, ?3)
             ON CONFLICT(canonical_path, config_hash)
             DO UPDATE SET last_used_at = excluded.last_used_at",
            params![canonical_path, config_hash, now],
        )?;
        Ok(())
    }

    /// Whether this exact canonical path and config hash are trusted.
    pub fn is_trusted(
        &self,
        canonical_path: &str,
        config_hash: &str,
    ) -> Result<bool, StorageError> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM workspace_trust
             WHERE canonical_path = ?1 AND config_hash = ?2",
            params![canonical_path, config_hash],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    /// Remove every trust record for a canonical path.
    pub fn remove_trust(&self, canonical_path: &str) -> Result<usize, StorageError> {
        self.conn
            .execute(
                "DELETE FROM workspace_trust WHERE canonical_path = ?1",
                params![canonical_path],
            )
            .map_err(Into::into)
    }

    /// List stored trust records.
    pub fn trust_records(&self) -> Result<Vec<TrustRecord>, StorageError> {
        let mut statement = self.conn.prepare(
            "SELECT canonical_path, config_hash, created_at, last_used_at
             FROM workspace_trust ORDER BY canonical_path ASC, created_at ASC",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(TrustRecord {
                canonical_path: row.get(0)?,
                config_hash: row.get(1)?,
                created_at: row.get(2)?,
                last_used_at: row.get(3)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }
}

/// Cloneable async wrapper around [`Storage`]. Blocking SQLite work runs on
/// Tokio's blocking pool so the async runtime is not stalled.
#[derive(Clone)]
pub struct StorageHandle {
    inner: Arc<Mutex<Storage>>,
}

impl StorageHandle {
    /// Open a database with the default redactor.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StorageError> {
        Ok(Self {
            inner: Arc::new(Mutex::new(Storage::open(path)?)),
        })
    }

    /// Open a database with a custom redactor.
    pub fn open_with_redactor(
        path: impl AsRef<Path>,
        redactor: Box<dyn Redactor>,
    ) -> Result<Self, StorageError> {
        Ok(Self {
            inner: Arc::new(Mutex::new(Storage::open_with_redactor(path, redactor)?)),
        })
    }

    async fn with<T, F>(&self, operation: F) -> Result<T, StorageError>
    where
        T: Send + 'static,
        F: FnOnce(&mut Storage) -> Result<T, StorageError> + Send + 'static,
    {
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || {
            let mut storage = inner.lock().map_err(|_| StorageError::Poisoned)?;
            operation(&mut storage)
        })
        .await
        .map_err(|error| StorageError::Join(error.to_string()))?
    }

    /// Insert a session record.
    pub async fn create_session(&self, session: SessionInfo) -> Result<(), StorageError> {
        self.with(move |storage| storage.create_session(&session))
            .await
    }

    /// Fetch one session.
    pub async fn get_session(&self, session_id: String) -> Result<SessionInfo, StorageError> {
        self.with(move |storage| storage.get_session(&session_id))
            .await
    }

    /// List sessions.
    pub async fn list_sessions(&self) -> Result<Vec<SessionInfo>, StorageError> {
        self.with(|storage| storage.list_sessions()).await
    }

    /// Update the active mode of a session.
    pub async fn update_session_mode(
        &self,
        session_id: String,
        mode: SessionMode,
    ) -> Result<(), StorageError> {
        self.with(move |storage| storage.update_session_mode(&session_id, mode))
            .await
    }

    /// List sessions, optionally including archived ones.
    pub async fn list_sessions_with_archived(
        &self,
        include_archived: bool,
    ) -> Result<Vec<SessionInfo>, StorageError> {
        self.with(move |storage| storage.list_sessions_with_archived(include_archived))
            .await
    }

    /// Rename a session.
    pub async fn rename_session(
        &self,
        session_id: String,
        name: String,
    ) -> Result<SessionInfo, StorageError> {
        self.with(move |storage| storage.rename_session(&session_id, &name))
            .await
    }

    /// Archive a session.
    pub async fn archive_session(&self, session_id: String) -> Result<SessionInfo, StorageError> {
        self.with(move |storage| storage.archive_session(&session_id))
            .await
    }

    /// Soft-delete a session.
    pub async fn delete_session(&self, session_id: String) -> Result<bool, StorageError> {
        self.with(move |storage| storage.delete_session(&session_id))
            .await
    }

    /// Update a session model policy.
    pub async fn update_session_model_policy(
        &self,
        session_id: String,
        policy: serde_json::Value,
    ) -> Result<SessionInfo, StorageError> {
        self.with(move |storage| storage.update_session_model_policy(&session_id, &policy))
            .await
    }

    /// Add turn usage to the session totals.
    pub async fn record_session_usage(
        &self,
        session_id: String,
        input_tokens: u64,
        output_tokens: u64,
        cost_usd: f64,
    ) -> Result<SessionInfo, StorageError> {
        self.with(move |storage| {
            storage.record_session_usage(&session_id, input_tokens, output_tokens, cost_usd)
        })
        .await
    }

    /// Update the session default model column.
    pub async fn update_session_model(
        &self,
        session_id: String,
        model: String,
    ) -> Result<SessionInfo, StorageError> {
        self.with(move |storage| storage.update_session_model(&session_id, &model))
            .await
    }

    /// Acquire or refresh a workspace lease.
    pub async fn acquire_lease(
        &self,
        canonical_workspace: String,
        session_id: String,
        ttl_secs: u64,
        steal: bool,
    ) -> Result<Option<WorkspaceLease>, StorageError> {
        self.with(move |storage| {
            storage.acquire_lease(&canonical_workspace, &session_id, ttl_secs, steal)
        })
        .await
    }

    /// Renew a workspace lease.
    pub async fn renew_lease(
        &self,
        canonical_workspace: String,
        session_id: String,
        ttl_secs: u64,
    ) -> Result<bool, StorageError> {
        self.with(move |storage| storage.renew_lease(&canonical_workspace, &session_id, ttl_secs))
            .await
    }

    /// Release a workspace lease.
    pub async fn release_lease(
        &self,
        canonical_workspace: String,
        session_id: String,
    ) -> Result<bool, StorageError> {
        self.with(move |storage| storage.release_lease(&canonical_workspace, &session_id))
            .await
    }

    /// Current lease holder.
    pub async fn lease_holder(
        &self,
        canonical_workspace: String,
    ) -> Result<Option<WorkspaceLease>, StorageError> {
        self.with(move |storage| storage.lease_holder(&canonical_workspace))
            .await
    }

    /// Persist a pending approval.
    pub async fn insert_pending_approval(
        &self,
        approval_id: String,
        session_id: String,
        action: serde_json::Value,
    ) -> Result<(), StorageError> {
        self.with(move |storage| {
            storage.insert_pending_approval(&approval_id, &session_id, &action)
        })
        .await
    }

    /// Resolve a pending approval.
    pub async fn resolve_approval(
        &self,
        approval_id: String,
        decision: String,
        reason: Option<String>,
    ) -> Result<bool, StorageError> {
        self.with(move |storage| {
            storage.resolve_approval(&approval_id, &decision, reason.as_deref())
        })
        .await
    }

    /// List pending approvals.
    pub async fn pending_approvals(
        &self,
        session_id: String,
    ) -> Result<Vec<ApprovalInfo>, StorageError> {
        self.with(move |storage| storage.pending_approvals(&session_id))
            .await
    }

    /// Append one event.
    pub async fn append_event(&self, event: NewEvent) -> Result<EventEnvelope, StorageError> {
        self.with(move |storage| storage.append_event(event)).await
    }

    /// Read events after a sequence number.
    pub async fn events_after(
        &self,
        session_id: String,
        after_seq: u64,
    ) -> Result<Vec<EventEnvelope>, StorageError> {
        self.with(move |storage| storage.events_after(&session_id, after_seq))
            .await
    }

    /// Last sequence number for a session.
    pub async fn last_seq(&self, session_id: String) -> Result<u64, StorageError> {
        self.with(move |storage| storage.last_seq(&session_id))
            .await
    }

    /// Search the FTS5 index.
    pub async fn search(
        &self,
        query: String,
        limit: u32,
    ) -> Result<Vec<EventEnvelope>, StorageError> {
        self.with(move |storage| storage.search(&query, limit))
            .await
    }

    /// Search with optional filters.
    pub async fn search_filtered(
        &self,
        query: String,
        session_id: Option<String>,
        workspace: Option<String>,
        limit: u32,
    ) -> Result<Vec<HistoryHit>, StorageError> {
        self.with(move |storage| {
            storage.search_filtered(&query, session_id.as_deref(), workspace.as_deref(), limit)
        })
        .await
    }

    /// Rebuild the FTS5 index.
    pub async fn rebuild_fts(&self) -> Result<(), StorageError> {
        self.with(|storage| storage.rebuild_fts()).await
    }

    /// Whether FTS5 is available.
    pub async fn has_fts5(&self) -> Result<bool, StorageError> {
        self.with(|storage| storage.has_fts5()).await
    }

    /// Current schema version.
    pub async fn schema_version(&self) -> Result<i64, StorageError> {
        self.with(|storage| storage.schema_version()).await
    }

    /// SQLite integrity check.
    pub async fn integrity_check(&self) -> Result<String, StorageError> {
        self.with(|storage| storage.integrity_check()).await
    }

    /// Record a workspace trust decision.
    pub async fn trust_workspace(
        &self,
        canonical_path: String,
        config_hash: String,
    ) -> Result<(), StorageError> {
        self.with(move |storage| storage.trust_workspace(&canonical_path, &config_hash))
            .await
    }

    /// Check a workspace trust decision.
    pub async fn is_trusted(
        &self,
        canonical_path: String,
        config_hash: String,
    ) -> Result<bool, StorageError> {
        self.with(move |storage| storage.is_trusted(&canonical_path, &config_hash))
            .await
    }

    /// Remove trust records for a canonical path.
    pub async fn remove_trust(&self, canonical_path: String) -> Result<usize, StorageError> {
        self.with(move |storage| storage.remove_trust(&canonical_path))
            .await
    }

    /// List trust records.
    pub async fn trust_records(&self) -> Result<Vec<TrustRecord>, StorageError> {
        self.with(|storage| storage.trust_records()).await
    }
}

struct SessionRow {
    session_id: String,
    parent_id: Option<String>,
    name: String,
    workspace: String,
    mode: String,
    model: String,
    model_policy_json: String,
    input_tokens: u64,
    output_tokens: u64,
    cost_usd: f64,
    status: String,
    created_at: String,
    updated_at: String,
}

impl SessionRow {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            session_id: row.get(0)?,
            parent_id: row.get(1)?,
            name: row.get(2)?,
            workspace: row.get(3)?,
            mode: row.get(4)?,
            model: row.get(5)?,
            model_policy_json: row.get(6)?,
            input_tokens: row.get::<_, i64>(7)? as u64,
            output_tokens: row.get::<_, i64>(8)? as u64,
            cost_usd: row.get(9)?,
            status: row.get(10)?,
            created_at: row.get(11)?,
            updated_at: row.get(12)?,
        })
    }

    fn into_session(self) -> Result<SessionInfo, StorageError> {
        Ok(SessionInfo {
            session_id: self.session_id,
            parent_id: self.parent_id,
            name: self.name,
            workspace: self.workspace,
            mode: SessionMode::parse(&self.mode)?,
            model: self.model,
            model_policy: serde_json::from_str(&self.model_policy_json)?,
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            cost_usd: self.cost_usd,
            status: SessionStatus::parse(&self.status)?,
            created_at: self.created_at,
            updated_at: self.updated_at,
        })
    }
}

struct EventRow {
    event_id: String,
    session_id: String,
    seq: i64,
    timestamp: String,
    kind: String,
    payload_json: String,
}

impl EventRow {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            event_id: row.get(0)?,
            session_id: row.get(1)?,
            seq: row.get(2)?,
            timestamp: row.get(3)?,
            kind: row.get(4)?,
            payload_json: row.get(5)?,
        })
    }

    fn into_event(self) -> Result<EventEnvelope, StorageError> {
        let seq = u64::try_from(self.seq).map_err(|_| StorageError::InvalidSequence(self.seq))?;
        Ok(EventEnvelope {
            event_id: self.event_id,
            session_id: self.session_id,
            seq,
            timestamp: self.timestamp,
            kind: EventKind::parse(&self.kind)?,
            payload: serde_json::from_str(&self.payload_json)?,
        })
    }
}

fn collect_search_text(value: &serde_json::Value) -> String {
    let mut output = String::new();
    collect_value(value, &mut output);
    output
}

fn collect_value(value: &serde_json::Value, output: &mut String) {
    match value {
        serde_json::Value::String(text) => {
            output.push_str(text);
            output.push(' ');
        }
        serde_json::Value::Number(number) => {
            output.push_str(&number.to_string());
            output.push(' ');
        }
        serde_json::Value::Bool(value) => {
            output.push_str(if *value { "true" } else { "false" });
            output.push(' ');
        }
        serde_json::Value::Array(values) => {
            for value in values {
                collect_value(value, output);
            }
        }
        serde_json::Value::Object(map) => {
            for (key, value) in map {
                output.push_str(key);
                output.push(' ');
                collect_value(value, output);
            }
        }
        serde_json::Value::Null => {}
    }
}

const MIGRATION_1: &str = r#"
CREATE TABLE IF NOT EXISTS sessions (
    session_id TEXT PRIMARY KEY,
    workspace TEXT NOT NULL,
    canonical_workspace TEXT NOT NULL,
    mode TEXT NOT NULL,
    model TEXT NOT NULL,
    status TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    archived_at TEXT
);

CREATE TABLE IF NOT EXISTS events (
    event_id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL REFERENCES sessions(session_id) ON DELETE RESTRICT,
    seq INTEGER NOT NULL,
    timestamp TEXT NOT NULL,
    kind TEXT NOT NULL,
    payload_json TEXT NOT NULL,
    search_text TEXT NOT NULL,
    UNIQUE(session_id, seq)
);

CREATE INDEX IF NOT EXISTS events_session_seq ON events(session_id, seq);

CREATE VIRTUAL TABLE IF NOT EXISTS events_fts USING fts5(
    event_id UNINDEXED,
    session_id UNINDEXED,
    kind UNINDEXED,
    search_text
);

CREATE TRIGGER IF NOT EXISTS events_no_update
BEFORE UPDATE ON events
BEGIN
    SELECT RAISE(ABORT, 'events are append-only');
END;

CREATE TRIGGER IF NOT EXISTS events_no_delete
BEFORE DELETE ON events
BEGIN
    SELECT RAISE(ABORT, 'events are append-only');
END;

CREATE TRIGGER IF NOT EXISTS events_after_insert
AFTER INSERT ON events
BEGIN
    INSERT INTO events_fts(event_id, session_id, kind, search_text)
    VALUES (new.event_id, new.session_id, new.kind, new.search_text);
END;

CREATE TABLE IF NOT EXISTS approvals (
    approval_id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL REFERENCES sessions(session_id) ON DELETE RESTRICT,
    event_id TEXT,
    action_hash TEXT NOT NULL,
    status TEXT NOT NULL,
    decision TEXT,
    created_at TEXT NOT NULL,
    resolved_at TEXT
);

CREATE TABLE IF NOT EXISTS snapshots (
    snapshot_id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL REFERENCES sessions(session_id) ON DELETE RESTRICT,
    event_id TEXT,
    kind TEXT NOT NULL,
    path TEXT NOT NULL,
    before_hash TEXT,
    after_hash TEXT,
    backup_path TEXT,
    created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS workspace_trust (
    canonical_path TEXT NOT NULL,
    config_hash TEXT NOT NULL,
    created_at TEXT NOT NULL,
    last_used_at TEXT NOT NULL,
    PRIMARY KEY (canonical_path, config_hash)
);
"#;

const MIGRATION_2: &str = r#"
ALTER TABLE sessions ADD COLUMN name TEXT NOT NULL DEFAULT '';
ALTER TABLE sessions ADD COLUMN model_policy_json TEXT NOT NULL DEFAULT '{}';
ALTER TABLE sessions ADD COLUMN deleted_at TEXT;
UPDATE sessions SET name = session_id WHERE name = '';
ALTER TABLE sessions ADD COLUMN input_tokens INTEGER NOT NULL DEFAULT 0;
ALTER TABLE sessions ADD COLUMN output_tokens INTEGER NOT NULL DEFAULT 0;
ALTER TABLE sessions ADD COLUMN cost_usd REAL NOT NULL DEFAULT 0;
ALTER TABLE approvals ADD COLUMN action_json TEXT;

CREATE TABLE IF NOT EXISTS workspace_leases (
    canonical_workspace TEXT PRIMARY KEY,
    session_id TEXT NOT NULL,
    acquired_at TEXT NOT NULL,
    heartbeat_at TEXT NOT NULL,
    expires_at TEXT NOT NULL
);
"#;

const MIGRATION_3: &str = r#"
ALTER TABLE sessions ADD COLUMN parent_id TEXT;
"#;
