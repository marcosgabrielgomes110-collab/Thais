//! Untrusted project configuration.
//!
//! `<repo>/.thais/config.toml` is repository content. It may carry
//! non-security preferences, but it can never define providers, base URLs,
//! credentials, permissions, verifier commands, or the sandbox level.

use std::path::{Path, PathBuf};

use thiserror::Error;

/// Parsed project configuration. The raw TOML is retained for non-security
/// preferences; sensitive keys are rejected before use.
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectConfig {
    values: toml::Value,
}

impl Default for ProjectConfig {
    fn default() -> Self {
        Self {
            values: toml::Value::Table(toml::map::Map::new()),
        }
    }
}

impl ProjectConfig {
    /// Load a project config. A missing file yields an empty configuration.
    pub fn load(path: &Path) -> Result<Self, ProjectConfigError> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(source) => {
                return Err(ProjectConfigError::Io {
                    path: path.to_path_buf(),
                    source,
                });
            }
        };
        let values: toml::Value = toml::from_str(&text)?;
        reject_security_keys(&values, "")?;
        Ok(Self { values })
    }

    /// Load the config for a project root.
    pub fn load_project(project: &crate::paths::ProjectPaths) -> Result<Self, ProjectConfigError> {
        Self::load(&project.config_path())
    }

    /// Raw TOML value, after security-key validation.
    #[must_use]
    pub fn values(&self) -> &toml::Value {
        &self.values
    }

    /// Whether the configuration is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        matches!(&self.values, toml::Value::Table(table) if table.is_empty())
    }

    /// Read a non-security preference by dotted path.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&toml::Value> {
        let mut value = &self.values;
        for segment in key.split('.') {
            value = value.get(segment)?;
        }
        Some(value)
    }
}

/// Project configuration errors.
#[derive(Debug, Error)]
pub enum ProjectConfigError {
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
    #[error("invalid project config: {0}")]
    Toml(#[from] toml::de::Error),
    /// A security-sensitive key was found.
    #[error("project config contains a security-sensitive key: {0}")]
    SecurityKey(String),
}

const SECURITY_KEYS: &[&str] = &[
    "api_key",
    "apikey",
    "auth",
    "authorization",
    "base_url",
    "model",
    "models",
    "password",
    "permission",
    "permissions",
    "policy",
    "private_key",
    "provider",
    "providers",
    "sandbox",
    "secret",
    "token",
    "verifier",
];

fn reject_security_keys(value: &toml::Value, path: &str) -> Result<(), ProjectConfigError> {
    match value {
        toml::Value::Table(table) => {
            for (key, value) in table {
                let full_path = if path.is_empty() {
                    key.clone()
                } else {
                    format!("{path}.{key}")
                };
                if is_security_key(key) {
                    return Err(ProjectConfigError::SecurityKey(full_path));
                }
                reject_security_keys(value, &full_path)?;
            }
        }
        toml::Value::Array(values) => {
            for (index, value) in values.iter().enumerate() {
                let full_path = if path.is_empty() {
                    index.to_string()
                } else {
                    format!("{path}.{index}")
                };
                reject_security_keys(value, &full_path)?;
            }
        }
        toml::Value::String(_)
        | toml::Value::Integer(_)
        | toml::Value::Float(_)
        | toml::Value::Boolean(_)
        | toml::Value::Datetime(_) => {}
    }
    Ok(())
}

fn is_security_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    SECURITY_KEYS.contains(&key.as_str())
        || key.ends_with("_api_key")
        || key.ends_with("_token")
        || key.ends_with("_secret")
        || key.ends_with("_password")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_is_empty() {
        let temp = tempfile::tempdir().expect("temp");
        let config = ProjectConfig::load(&temp.path().join("missing.toml")).expect("load");
        assert!(config.is_empty());
    }

    #[test]
    fn non_security_preferences_load() {
        let temp = tempfile::tempdir().expect("temp");
        let path = temp.path().join("config.toml");
        std::fs::write(&path, "[ui]\ntheme = \"dark\"\n").expect("write");

        let config = ProjectConfig::load(&path).expect("load");
        assert_eq!(
            config.get("ui.theme").and_then(toml::Value::as_str),
            Some("dark")
        );
    }

    #[test]
    fn security_keys_are_rejected() {
        for body in [
            "base_url = \"https://evil.example\"\n",
            "[providers]\nid = \"evil\"\n",
            "[ui]\ntoken = \"secret\"\n",
            "sandbox = \"off\"\n",
            "[verifier]\ncommand = \"rm -rf /\"\n",
        ] {
            let temp = tempfile::tempdir().expect("temp");
            let path = temp.path().join("config.toml");
            std::fs::write(&path, body).expect("write");
            assert!(
                matches!(
                    ProjectConfig::load(&path),
                    Err(ProjectConfigError::SecurityKey(_))
                ),
                "body should be rejected: {body}"
            );
        }
    }
}
