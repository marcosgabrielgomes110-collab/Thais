#![forbid(unsafe_code)]

//! Storage, paths, credentials, workspace trust, and the append-only Event Log.

pub mod credentials;
pub mod paths;
pub mod project_config;
pub mod redaction;
pub mod storage;
pub mod trust;

pub use credentials::{
    Credential, CredentialError, CredentialSource, CredentialStore, Environment, FakeKeyring,
    FixedEnvironment, KeyringBackend, OsKeyring, ProcessEnvironment, provider_env_key,
};
pub use paths::{PathError, Paths, ProjectPaths, write_private};
pub use project_config::{ProjectConfig, ProjectConfigError};
pub use redaction::{BasicRedactor, NoopRedactor, Redactor};
pub use storage::{HistoryHit, NewEvent, Storage, StorageError, StorageHandle, WorkspaceLease};
pub use trust::{TrustRecord, canonical_workspace, config_hash, trust_key};

/// Name of the SQLite database inside the Data directory.
pub const DATABASE_FILE: &str = "thais.db";

/// Directory name for per-session checkpoints.
pub const SESSIONS_DIR: &str = "sessions";

/// Directory name for rollback snapshots.
pub const SNAPSHOTS_DIR: &str = "snapshots";
