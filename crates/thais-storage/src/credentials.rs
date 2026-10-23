//! Credential lookup and storage.
//!
//! Precedence is environment, then the OS keyring, then the `0600` fallback
//! file. The fallback is only written when the keyring is unavailable, and the
//! caller is warned when it is used.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::paths::{PathError, Paths, write_private};

/// Keyring service name.
pub const SERVICE: &str = "thais";

/// Where a credential was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialSource {
    /// Environment variable.
    Environment,
    /// OS keyring.
    Keyring,
    /// `0600` fallback file.
    File,
}

impl CredentialSource {
    /// Human-readable name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Environment => "environment",
            Self::Keyring => "keyring",
            Self::File => "file",
        }
    }
}

/// A resolved credential. The secret is never printed by the CLI.
#[derive(Debug, Clone)]
pub struct Credential {
    /// Secret value.
    pub secret: String,
    /// Where the value came from.
    pub source: CredentialSource,
}

impl Credential {
    /// Stable fingerprint that does not reveal the secret.
    #[must_use]
    pub fn fingerprint(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(self.secret.as_bytes());
        let digest = hex::encode(hasher.finalize());
        format!("sha256:{}", &digest[..12])
    }

    /// Last four characters, or `****` for short secrets.
    #[must_use]
    pub fn last_four(&self) -> String {
        let count = self.secret.chars().count();
        if count <= 4 {
            return "****".to_owned();
        }
        self.secret.chars().skip(count - 4).collect::<String>()
    }
}

/// Keyring backend abstraction.
pub trait KeyringBackend: Send + Sync {
    /// Read a secret, or `None` when the entry does not exist.
    fn get(&self, service: &str, account: &str) -> Result<Option<String>, CredentialError>;
    /// Write a secret.
    fn set(&self, service: &str, account: &str, secret: &str) -> Result<(), CredentialError>;
    /// Delete a secret; returns whether an entry existed.
    fn delete(&self, service: &str, account: &str) -> Result<bool, CredentialError>;
    /// Backend name for diagnostics.
    fn name(&self) -> &'static str;
    /// Whether the backend can be used.
    fn available(&self) -> bool;
}

/// OS keyring backend.
#[derive(Debug, Default, Clone, Copy)]
pub struct OsKeyring;

impl KeyringBackend for OsKeyring {
    fn get(&self, service: &str, account: &str) -> Result<Option<String>, CredentialError> {
        let entry = keyring::Entry::new(service, account)
            .map_err(|error| CredentialError::Keyring(error.to_string()))?;
        match entry.get_password() {
            Ok(secret) => Ok(Some(secret)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(CredentialError::Keyring(error.to_string())),
        }
    }

    fn set(&self, service: &str, account: &str, secret: &str) -> Result<(), CredentialError> {
        let entry = keyring::Entry::new(service, account)
            .map_err(|error| CredentialError::Keyring(error.to_string()))?;
        entry
            .set_password(secret)
            .map_err(|error| CredentialError::Keyring(error.to_string()))
    }

    fn delete(&self, service: &str, account: &str) -> Result<bool, CredentialError> {
        let entry = keyring::Entry::new(service, account)
            .map_err(|error| CredentialError::Keyring(error.to_string()))?;
        match entry.delete_credential() {
            Ok(()) => Ok(true),
            Err(keyring::Error::NoEntry) => Ok(false),
            Err(error) => Err(CredentialError::Keyring(error.to_string())),
        }
    }

    fn name(&self) -> &'static str {
        "os"
    }

    fn available(&self) -> bool {
        keyring::Entry::store_status().is_ok()
    }
}

/// In-memory keyring for tests.
#[derive(Debug, Clone, Default)]
pub struct FakeKeyring {
    values: Arc<Mutex<HashMap<String, String>>>,
    available: bool,
}

impl FakeKeyring {
    /// Build an available fake keyring.
    #[must_use]
    pub fn new() -> Self {
        Self {
            values: Arc::new(Mutex::new(HashMap::new())),
            available: true,
        }
    }

    /// Build an unavailable fake keyring.
    #[must_use]
    pub fn unavailable() -> Self {
        Self {
            values: Arc::new(Mutex::new(HashMap::new())),
            available: false,
        }
    }

    /// Seed an entry.
    pub fn seed(&self, service: &str, account: &str, secret: &str) {
        if let Ok(mut values) = self.values.lock() {
            values.insert(format!("{service}:{account}"), secret.to_owned());
        }
    }
}

impl KeyringBackend for FakeKeyring {
    fn get(&self, service: &str, account: &str) -> Result<Option<String>, CredentialError> {
        if !self.available {
            return Err(CredentialError::Keyring(
                "fake keyring unavailable".to_owned(),
            ));
        }
        let values = self
            .values
            .lock()
            .map_err(|_| CredentialError::Keyring("fake keyring poisoned".to_owned()))?;
        Ok(values.get(&format!("{service}:{account}")).cloned())
    }

    fn set(&self, service: &str, account: &str, secret: &str) -> Result<(), CredentialError> {
        if !self.available {
            return Err(CredentialError::Keyring(
                "fake keyring unavailable".to_owned(),
            ));
        }
        let mut values = self
            .values
            .lock()
            .map_err(|_| CredentialError::Keyring("fake keyring poisoned".to_owned()))?;
        values.insert(format!("{service}:{account}"), secret.to_owned());
        Ok(())
    }

    fn delete(&self, service: &str, account: &str) -> Result<bool, CredentialError> {
        if !self.available {
            return Err(CredentialError::Keyring(
                "fake keyring unavailable".to_owned(),
            ));
        }
        let mut values = self
            .values
            .lock()
            .map_err(|_| CredentialError::Keyring("fake keyring poisoned".to_owned()))?;
        Ok(values.remove(&format!("{service}:{account}")).is_some())
    }

    fn name(&self) -> &'static str {
        "fake"
    }

    fn available(&self) -> bool {
        self.available
    }
}

/// Environment lookup abstraction.
pub trait Environment: Send + Sync {
    /// Read an environment variable.
    fn var(&self, key: &str) -> Option<String>;
}

/// Process environment.
#[derive(Debug, Default, Clone, Copy)]
pub struct ProcessEnvironment;

impl Environment for ProcessEnvironment {
    fn var(&self, key: &str) -> Option<String> {
        std::env::var(key).ok()
    }
}

/// Fixed environment for tests.
#[derive(Debug, Clone, Default)]
pub struct FixedEnvironment {
    values: HashMap<String, String>,
}

impl FixedEnvironment {
    /// Build a fixed environment from pairs.
    #[must_use]
    pub fn new(values: impl IntoIterator<Item = (String, String)>) -> Self {
        Self {
            values: values.into_iter().collect(),
        }
    }
}

impl Environment for FixedEnvironment {
    fn var(&self, key: &str) -> Option<String> {
        self.values.get(key).cloned()
    }
}

/// Credential store with environment, keyring, and file backends.
pub struct CredentialStore {
    paths: Paths,
    keyring: Box<dyn KeyringBackend>,
    environment: Box<dyn Environment>,
}

impl CredentialStore {
    /// Build a store using the OS keyring and process environment.
    #[must_use]
    pub fn new(paths: Paths) -> Self {
        Self {
            paths,
            keyring: Box::new(OsKeyring),
            environment: Box::new(ProcessEnvironment),
        }
    }

    /// Build a store with explicit backends.
    #[must_use]
    pub fn with_backends(
        paths: Paths,
        keyring: Box<dyn KeyringBackend>,
        environment: Box<dyn Environment>,
    ) -> Self {
        Self {
            paths,
            keyring,
            environment,
        }
    }

    /// Resolve a credential using the documented precedence.
    pub fn get(
        &self,
        provider: &str,
        account: &str,
    ) -> Result<Option<Credential>, CredentialError> {
        let environment_key = provider_env_key(provider);
        if let Some(secret) = self
            .environment
            .var(&environment_key)
            .filter(|secret| !secret.is_empty())
        {
            return Ok(Some(Credential {
                secret,
                source: CredentialSource::Environment,
            }));
        }

        let account_key = format!("{provider}:{account}");
        let keyring_error = match self.keyring.get(SERVICE, &account_key) {
            Ok(Some(secret)) if !secret.is_empty() => {
                return Ok(Some(Credential {
                    secret,
                    source: CredentialSource::Keyring,
                }));
            }
            Ok(_) => None,
            Err(error) => Some(error),
        };

        match self.read_file_entry(&account_key) {
            Ok(Some(secret)) => Ok(Some(Credential {
                secret,
                source: CredentialSource::File,
            })),
            Ok(None) => {
                if let Some(error) = keyring_error {
                    tracing::warn!(
                        provider,
                        account,
                        error = %error,
                        "keyring unavailable and no fallback credential was found"
                    );
                }
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    /// Store a credential in the keyring, falling back to the `0600` file.
    pub fn set(
        &self,
        provider: &str,
        account: &str,
        secret: &str,
    ) -> Result<CredentialSource, CredentialError> {
        let account_key = format!("{provider}:{account}");
        match self.keyring.set(SERVICE, &account_key, secret) {
            Ok(()) => Ok(CredentialSource::Keyring),
            Err(error) => {
                tracing::warn!(
                    provider,
                    account,
                    error = %error,
                    "keyring unavailable; writing the 0600 credential fallback"
                );
                self.write_file_entry(&account_key, secret)?;
                Ok(CredentialSource::File)
            }
        }
    }

    /// Delete a credential from both backends.
    pub fn delete(&self, provider: &str, account: &str) -> Result<bool, CredentialError> {
        let account_key = format!("{provider}:{account}");
        let mut removed = false;
        match self.keyring.delete(SERVICE, &account_key) {
            Ok(existed) => removed |= existed,
            Err(error) => {
                tracing::warn!(provider, account, error = %error, "keyring delete failed");
            }
        }
        removed |= self.remove_file_entry(&account_key)?;
        Ok(removed)
    }

    /// Keyring backend name.
    #[must_use]
    pub fn keyring_name(&self) -> &'static str {
        self.keyring.name()
    }

    /// Whether the keyring backend is available.
    #[must_use]
    pub fn keyring_available(&self) -> bool {
        self.keyring.available()
    }

    /// Whether the fallback file exists.
    #[must_use]
    pub fn fallback_file_exists(&self) -> bool {
        self.paths.credentials_path().exists()
    }

    fn read_file_entry(&self, account_key: &str) -> Result<Option<String>, CredentialError> {
        let path = self.paths.credentials_path();
        if !path.exists() {
            return Ok(None);
        }
        let text = std::fs::read_to_string(&path).map_err(|source| CredentialError::Io {
            path: path.clone(),
            source,
        })?;
        let value: toml::Value = toml::from_str(&text)?;
        Ok(value
            .get("credentials")
            .and_then(toml::Value::as_table)
            .and_then(|credentials| credentials.get(account_key))
            .and_then(toml::Value::as_str)
            .map(ToOwned::to_owned))
    }

    fn write_file_entry(&self, account_key: &str, secret: &str) -> Result<(), CredentialError> {
        let path = self.paths.credentials_path();
        let mut value = if path.exists() {
            let text = std::fs::read_to_string(&path).map_err(|source| CredentialError::Io {
                path: path.clone(),
                source,
            })?;
            toml::from_str(&text)?
        } else {
            toml::Value::Table(toml::map::Map::new())
        };

        let table = value.as_table_mut().ok_or_else(|| {
            CredentialError::InvalidFormat("credentials file root is not a table".to_owned())
        })?;
        let credentials = table
            .entry("credentials")
            .or_insert_with(|| toml::Value::Table(toml::map::Map::new()))
            .as_table_mut()
            .ok_or_else(|| {
                CredentialError::InvalidFormat("credentials entry is not a table".to_owned())
            })?;
        credentials.insert(
            account_key.to_owned(),
            toml::Value::String(secret.to_owned()),
        );

        let serialized = toml::to_string_pretty(&value)?;
        write_private(&path, serialized.as_bytes())?;
        Ok(())
    }

    fn remove_file_entry(&self, account_key: &str) -> Result<bool, CredentialError> {
        let path = self.paths.credentials_path();
        if !path.exists() {
            return Ok(false);
        }
        let text = std::fs::read_to_string(&path).map_err(|source| CredentialError::Io {
            path: path.clone(),
            source,
        })?;
        let mut value: toml::Value = toml::from_str(&text)?;
        let Some(table) = value.as_table_mut() else {
            return Ok(false);
        };
        let Some(credentials) = table
            .get_mut("credentials")
            .and_then(toml::Value::as_table_mut)
        else {
            return Ok(false);
        };
        let removed = credentials.remove(account_key).is_some();
        if removed {
            if credentials.is_empty() {
                std::fs::remove_file(&path).map_err(|source| CredentialError::Io {
                    path: path.clone(),
                    source,
                })?;
            } else {
                let serialized = toml::to_string_pretty(&value)?;
                write_private(&path, serialized.as_bytes())?;
            }
        }
        Ok(removed)
    }
}

/// Build the environment variable name for a provider.
#[must_use]
pub fn provider_env_key(provider: &str) -> String {
    let suffix: String = provider
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect();
    format!("THAIS_{suffix}_API_KEY")
}

/// Credential errors.
#[derive(Debug, Error)]
pub enum CredentialError {
    /// Keyring backend failure.
    #[error("keyring error: {0}")]
    Keyring(String),
    /// I/O failure.
    #[error("I/O error at {path}: {source}")]
    Io {
        /// Path involved in the failure.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },
    /// TOML parse failure.
    #[error("invalid credentials file: {0}")]
    Toml(#[from] toml::de::Error),
    /// TOML serialization failure.
    #[error("could not serialize credentials file: {0}")]
    TomlSerialize(#[from] toml::ser::Error),
    /// The file has an unexpected shape.
    #[error("invalid credentials file: {0}")]
    InvalidFormat(String),
    /// Path resolution failure.
    #[error(transparent)]
    Path(#[from] PathError),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths_for(root: &std::path::Path) -> Paths {
        Paths::from_roots(
            root.join("config"),
            root.join("data"),
            root.join("state"),
            root.join("cache"),
        )
    }

    #[test]
    fn environment_wins_over_keyring_and_file() {
        let temp = tempfile::tempdir().expect("temp");
        let paths = paths_for(temp.path());
        paths.ensure().expect("ensure");
        let keyring = FakeKeyring::new();
        keyring.seed(SERVICE, "openai:default", "from-keyring");
        let environment =
            FixedEnvironment::new([(provider_env_key("openai"), "from-environment".to_owned())]);
        let store = CredentialStore::with_backends(paths, Box::new(keyring), Box::new(environment));

        let credential = store
            .get("openai", "default")
            .expect("get")
            .expect("credential");
        assert_eq!(credential.secret, "from-environment");
        assert_eq!(credential.source, CredentialSource::Environment);
    }

    #[test]
    fn keyring_wins_over_file() {
        let temp = tempfile::tempdir().expect("temp");
        let paths = paths_for(temp.path());
        paths.ensure().expect("ensure");
        let keyring = FakeKeyring::new();
        keyring.seed(SERVICE, "openai:default", "from-keyring");
        let store = CredentialStore::with_backends(
            paths,
            Box::new(keyring),
            Box::new(FixedEnvironment::default()),
        );

        let credential = store
            .get("openai", "default")
            .expect("get")
            .expect("credential");
        assert_eq!(credential.secret, "from-keyring");
        assert_eq!(credential.source, CredentialSource::Keyring);
    }

    #[test]
    fn file_fallback_is_used_when_keyring_is_unavailable() {
        let temp = tempfile::tempdir().expect("temp");
        let paths = paths_for(temp.path());
        paths.ensure().expect("ensure");
        let keyring = FakeKeyring::unavailable();
        let store = CredentialStore::with_backends(
            paths.clone(),
            Box::new(keyring),
            Box::new(FixedEnvironment::default()),
        );
        let source = store.set("openai", "default", "file-secret").expect("set");
        assert_eq!(source, CredentialSource::File);

        let credential = store
            .get("openai", "default")
            .expect("get")
            .expect("credential");
        assert_eq!(credential.secret, "file-secret");
        assert_eq!(credential.source, CredentialSource::File);
        assert!(store.fallback_file_exists());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(paths.credentials_path())
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn delete_removes_keyring_and_file_entries() {
        let temp = tempfile::tempdir().expect("temp");
        let paths = paths_for(temp.path());
        paths.ensure().expect("ensure");
        let keyring = FakeKeyring::new();
        keyring.seed(SERVICE, "deepseek:work", "keyring-secret");
        let store = CredentialStore::with_backends(
            paths,
            Box::new(keyring),
            Box::new(FixedEnvironment::default()),
        );
        store
            .set("deepseek", "work", "keyring-secret")
            .expect("set keyring");
        assert!(store.delete("deepseek", "work").expect("delete keyring"));
        assert!(
            store
                .get("deepseek", "work")
                .expect("get keyring")
                .is_none()
        );

        let temp2 = tempfile::tempdir().expect("temp");
        let paths2 = paths_for(temp2.path());
        paths2.ensure().expect("ensure");
        let store2 = CredentialStore::with_backends(
            paths2,
            Box::new(FakeKeyring::unavailable()),
            Box::new(FixedEnvironment::default()),
        );
        store2
            .set("openai", "default", "file-secret")
            .expect("set file");

        assert!(store2.delete("openai", "default").expect("delete file"));
        assert!(store2.get("openai", "default").expect("get file").is_none());
    }

    #[test]
    fn fingerprint_does_not_reveal_the_secret() {
        let credential = Credential {
            secret: "sk-super-secret-value".to_owned(),
            source: CredentialSource::Keyring,
        };
        let fingerprint = credential.fingerprint();
        assert!(fingerprint.starts_with("sha256:"));
        assert!(!fingerprint.contains("super"));
        assert_eq!(credential.last_four(), "alue");
    }

    #[test]
    fn provider_env_key_is_uppercase_and_safe() {
        assert_eq!(provider_env_key("openai"), "THAIS_OPENAI_API_KEY");
        assert_eq!(
            provider_env_key("openai-compatible"),
            "THAIS_OPENAI_COMPATIBLE_API_KEY"
        );
    }
}
