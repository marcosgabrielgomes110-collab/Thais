//! Permission policy for read-only and mutating tools.

use serde_json::Value;

use crate::path_jail::PathJail;

/// Session policy mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyMode {
    /// Read-only planning.
    Plan,
    /// Build mode; mutations are allowed after approval.
    Build,
}

/// Gate decision for a proposed tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionDecision {
    /// Execute the tool.
    Allow,
    /// Ask the user; the string explains why.
    Ask(String),
    /// Deny the tool; the string is returned to the model.
    Deny(String),
}

/// Permission policy.
#[derive(Debug, Clone, Copy)]
pub struct PermissionGate {
    mode: PolicyMode,
}

impl PermissionGate {
    /// Build a gate for a mode.
    #[must_use]
    pub fn new(mode: PolicyMode) -> Self {
        Self { mode }
    }

    /// Check a tool call against the active mode and policy.
    #[must_use]
    pub fn check(
        &self,
        tool: &str,
        args: &Value,
        jail: &PathJail,
        mutating: bool,
        summary: &str,
    ) -> PermissionDecision {
        const KNOWN: &[&str] = &[
            "read",
            "list",
            "glob",
            "grep",
            "git_status",
            "git_diff",
            "write",
            "edit",
            "apply_patch",
            "bash",
            "memory_write",
        ];
        if !KNOWN.contains(&tool) {
            return PermissionDecision::Deny(format!("tool `{tool}` is not available"));
        }

        if mutating {
            if self.mode == PolicyMode::Plan {
                return PermissionDecision::Deny(
                    "plan mode is read-only; approve the plan before changing files".to_owned(),
                );
            }
            return PermissionDecision::Ask(summary.to_owned());
        }

        for key in ["path", "cwd"] {
            if let Some(path) = args.get(key).and_then(Value::as_str) {
                let resolved = match jail.resolve(path) {
                    Ok(resolved) => resolved,
                    Err(error) => return PermissionDecision::Deny(error.to_string()),
                };
                if is_git_internal(&resolved, jail) {
                    return PermissionDecision::Deny(
                        "git internals are not readable in M2".to_owned(),
                    );
                }
                if is_sensitive(&resolved) {
                    return PermissionDecision::Ask(format!(
                        "read sensitive file {}",
                        jail.display(&resolved)
                    ));
                }
            }
        }

        PermissionDecision::Allow
    }
}

fn is_git_internal(path: &std::path::Path, jail: &PathJail) -> bool {
    path.strip_prefix(jail.root()).ok().is_some_and(|relative| {
        relative
            .components()
            .any(|component| component.as_os_str() == ".git")
    })
}

fn is_sensitive(path: &std::path::Path) -> bool {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    name == ".env"
        || name.starts_with(".env.")
        || name == "credentials.toml"
        || name == "id_rsa"
        || name == "id_ed25519"
        || name == ".netrc"
        || name == ".npmrc"
        || name.ends_with(".pem")
        || name.ends_with(".key")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn normal_reads_are_allowed_in_build_mode() {
        let temp = tempfile::tempdir().expect("temp");
        std::fs::write(temp.path().join("src.rs"), "fn main() {}").expect("file");
        let jail = PathJail::new(temp.path()).expect("jail");
        assert_eq!(
            PermissionGate::new(PolicyMode::Build).check(
                "read",
                &json!({"path": "src.rs"}),
                &jail,
                false,
                "read src.rs"
            ),
            PermissionDecision::Allow
        );
    }

    #[test]
    fn sensitive_files_are_asked() {
        let temp = tempfile::tempdir().expect("temp");
        std::fs::write(temp.path().join(".env"), "TOKEN=x").expect("file");
        let jail = PathJail::new(temp.path()).expect("jail");
        assert!(matches!(
            PermissionGate::new(PolicyMode::Build).check(
                "read",
                &json!({"path": ".env"}),
                &jail,
                false,
                "read .env"
            ),
            PermissionDecision::Ask(_)
        ));
    }

    #[test]
    fn plan_mode_denies_mutations_and_build_mode_asks() {
        let temp = tempfile::tempdir().expect("temp");
        let jail = PathJail::new(temp.path()).expect("jail");
        assert!(matches!(
            PermissionGate::new(PolicyMode::Plan).check(
                "write",
                &json!({"path": "file.txt"}),
                &jail,
                true,
                "write file.txt"
            ),
            PermissionDecision::Deny(_)
        ));
        assert!(matches!(
            PermissionGate::new(PolicyMode::Build).check(
                "write",
                &json!({"path": "file.txt"}),
                &jail,
                true,
                "write file.txt"
            ),
            PermissionDecision::Ask(_)
        ));
        assert!(matches!(
            PermissionGate::new(PolicyMode::Build).check(
                "bash",
                &json!({"command": "ls"}),
                &jail,
                true,
                "bash: ls"
            ),
            PermissionDecision::Ask(_)
        ));
    }

    #[test]
    fn escapes_and_git_internals_are_denied() {
        let temp = tempfile::tempdir().expect("temp");
        std::fs::create_dir_all(temp.path().join(".git")).expect("git dir");
        std::fs::write(temp.path().join(".git/config"), "secret").expect("config");
        let jail = PathJail::new(temp.path()).expect("jail");

        assert!(matches!(
            PermissionGate::new(PolicyMode::Build).check(
                "read",
                &json!({"path": "/etc/passwd"}),
                &jail,
                false,
                "read"
            ),
            PermissionDecision::Deny(_)
        ));
        assert!(matches!(
            PermissionGate::new(PolicyMode::Build).check(
                "read",
                &json!({"path": ".git/config"}),
                &jail,
                false,
                "read"
            ),
            PermissionDecision::Deny(_)
        ));
        assert!(matches!(
            PermissionGate::new(PolicyMode::Build).check(
                "unknown",
                &json!({}),
                &jail,
                false,
                "unknown"
            ),
            PermissionDecision::Deny(_)
        ));
    }
}
