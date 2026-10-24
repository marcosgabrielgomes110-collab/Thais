//! Verifier commands loaded from user configuration.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use serde_json::json;
use thais_tools::{BashTool, PathJail, Tool};
use thiserror::Error;

/// Verification outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verification {
    /// Every command exited zero.
    Pass,
    /// At least one command failed.
    Fail {
        /// Combined command output.
        output: String,
    },
    /// Verification could not run.
    Unavailable {
        /// Why verification is unavailable.
        reason: String,
    },
}

/// Verifier built from user config. Project config never supplies these commands.
#[derive(Debug, Clone)]
pub struct Verifier {
    commands: Vec<String>,
    timeout: Duration,
}

impl Verifier {
    /// Build a verifier with explicit commands.
    #[must_use]
    pub fn new(commands: Vec<String>, timeout: Duration) -> Self {
        Self { commands, timeout }
    }

    /// Build an empty verifier.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            commands: Vec::new(),
            timeout: Duration::from_secs(120),
        }
    }

    /// Load `verifier.toml`. Missing files yield an empty verifier.
    pub fn load(path: &Path) -> Result<Self, VerifierError> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Self::empty()),
            Err(source) => {
                return Err(VerifierError::Io {
                    path: path.to_path_buf(),
                    source,
                });
            }
        };
        let config: VerifierConfig = toml::from_str(&text)?;
        Ok(Self {
            commands: config.commands,
            timeout: Duration::from_secs(config.timeout_secs.max(1)),
        })
    }

    /// Configured commands.
    #[must_use]
    pub fn commands(&self) -> &[String] {
        &self.commands
    }

    /// Run all verifier commands in the workspace.
    #[must_use]
    pub fn run(&self, workspace: &Path) -> Verification {
        if self.commands.is_empty() {
            return Verification::Unavailable {
                reason: "no verifier commands configured".to_owned(),
            };
        }
        let jail = match PathJail::new(workspace) {
            Ok(jail) => jail,
            Err(error) => {
                return Verification::Unavailable {
                    reason: error.to_string(),
                };
            }
        };
        let timeout_ms = self.timeout.as_millis().min(120_000) as u64;
        let tool = BashTool;
        let mut output = String::new();
        for command in &self.commands {
            let result =
                match tool.execute(json!({"command": command, "timeout_ms": timeout_ms}), &jail) {
                    Ok(result) => result,
                    Err(error) => {
                        return Verification::Unavailable {
                            reason: error.to_string(),
                        };
                    }
                };
            output.push_str(&format!("$ {command}\n{}\n", result.content));
            if exit_code(&result.content) != Some(0) {
                return Verification::Fail { output };
            }
        }
        Verification::Pass
    }
}

fn exit_code(content: &str) -> Option<i32> {
    content
        .lines()
        .find_map(|line| line.strip_prefix("exit_code:")?.trim().parse::<i32>().ok())
}

#[derive(Debug, Deserialize)]
struct VerifierConfig {
    #[serde(default)]
    commands: Vec<String>,
    #[serde(default = "default_timeout")]
    timeout_secs: u64,
}

fn default_timeout() -> u64 {
    120
}

/// Verifier configuration errors.
#[derive(Debug, Error)]
pub enum VerifierError {
    /// I/O failure.
    #[error("verifier I/O error at {path}: {source}")]
    Io {
        /// Path involved.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },
    /// Invalid TOML.
    #[error("invalid verifier config: {0}")]
    Toml(#[from] toml::de::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passing_and_failing_commands() {
        let temp = tempfile::tempdir().expect("temp");
        let pass = Verifier::new(vec!["true".to_owned()], Duration::from_secs(5));
        assert_eq!(pass.run(temp.path()), Verification::Pass);

        let fail = Verifier::new(vec!["false".to_owned()], Duration::from_secs(5));
        assert!(matches!(fail.run(temp.path()), Verification::Fail { .. }));
    }

    #[test]
    fn empty_verifier_is_unavailable() {
        let temp = tempfile::tempdir().expect("temp");
        assert!(matches!(
            Verifier::empty().run(temp.path()),
            Verification::Unavailable { .. }
        ));
    }

    #[test]
    fn loads_commands_from_toml() {
        let temp = tempfile::tempdir().expect("temp");
        let path = temp.path().join("verifier.toml");
        std::fs::write(&path, "commands = [\"true\"]\ntimeout_secs = 5\n").expect("config");
        let verifier = Verifier::load(&path).expect("load");
        assert_eq!(verifier.commands(), ["true"]);
    }
}
