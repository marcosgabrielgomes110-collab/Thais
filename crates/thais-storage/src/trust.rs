//! Workspace trust keys: canonical path plus configuration hash.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::storage::StorageError;

/// Canonicalize a workspace path, resolving symlinks and relative segments.
pub fn canonical_workspace(path: &Path) -> Result<PathBuf, StorageError> {
    std::fs::canonicalize(path).map_err(|source| StorageError::Io {
        path: path.to_path_buf(),
        source,
    })
}

/// Hash the project configuration file. A missing file hashes as empty bytes.
pub fn config_hash(config_path: &Path) -> Result<String, StorageError> {
    let bytes = match std::fs::read(config_path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == ErrorKind::NotFound => Vec::new(),
        Err(source) => {
            return Err(StorageError::Io {
                path: config_path.to_path_buf(),
                source,
            });
        }
    };

    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Ok(hex::encode(hasher.finalize()))
}

/// Derive the trust key for a workspace.
pub fn trust_key(workspace: &Path) -> Result<(String, String), StorageError> {
    let canonical = canonical_workspace(workspace)?;
    let config_path = canonical.join(".thais").join("config.toml");
    let hash = config_hash(&config_path)?;
    let canonical_string = canonical
        .to_str()
        .ok_or_else(|| StorageError::InvalidPath(canonical.clone()))?
        .to_owned();
    Ok((canonical_string, hash))
}

/// A stored trust decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustRecord {
    /// Canonical workspace path.
    pub canonical_path: String,
    /// Hash of `.thais/config.toml` at the time of trust.
    pub config_hash: String,
    /// RFC 3339 creation timestamp.
    pub created_at: String,
    /// RFC 3339 last-use timestamp.
    pub last_used_at: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_hash_changes_with_content() {
        let dir = tempfile::tempdir().expect("temp dir");
        let config = dir.path().join("config.toml");
        let empty = config_hash(&config).expect("empty hash");

        std::fs::write(&config, "foo = 1\n").expect("write");
        let populated = config_hash(&config).expect("hash");
        assert_ne!(empty, populated);

        std::fs::write(&config, "foo = 2\n").expect("write");
        let changed = config_hash(&config).expect("hash");
        assert_ne!(populated, changed);
    }

    #[test]
    fn trust_key_uses_canonical_path_and_config_hash() {
        let dir = tempfile::tempdir().expect("temp dir");
        let workspace = dir.path();
        std::fs::create_dir_all(workspace.join(".thais")).expect("config dir");
        std::fs::write(workspace.join(".thais/config.toml"), "theme = \"dark\"\n")
            .expect("write config");

        let (path, hash) = trust_key(workspace).expect("trust key");
        assert_eq!(
            path,
            workspace
                .canonicalize()
                .expect("canonical")
                .to_string_lossy()
        );
        assert_eq!(hash.len(), 64);
    }
}
