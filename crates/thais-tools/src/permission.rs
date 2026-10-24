//! M1 permission policy for read-only tools.

use serde_json::Value;

use crate::path_jail::PathJail;

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

/// Read-only permission policy.
#[derive(Debug, Default, Clone, Copy)]
pub struct PermissionGate;

impl PermissionGate {
    /// Check a tool call against the M1 policy.
    #[must_use]
    pub fn check(&self, tool: &str, args: &Value, jail: &PathJail) -> PermissionDecision {
        if !matches!(tool, "read" | "list" | "glob" | "grep") {
            return PermissionDecision::Deny(format!("tool `{tool}` is not available in M1"));
        }

        if let Some(path) = args.get("path").and_then(Value::as_str) {
            let resolved = match jail.resolve(path) {
                Ok(resolved) => resolved,
                Err(error) => return PermissionDecision::Deny(error.to_string()),
            };
            if is_git_internal(&resolved, jail) {
                return PermissionDecision::Deny("git internals are not readable in M1".to_owned());
            }
            if is_sensitive(&resolved) {
                return PermissionDecision::Ask(format!(
                    "read sensitive file {}",
                    jail.display(&resolved)
                ));
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
    fn normal_reads_are_allowed() {
        let temp = tempfile::tempdir().expect("temp");
        std::fs::write(temp.path().join("src.rs"), "fn main() {}").expect("file");
        let jail = PathJail::new(temp.path()).expect("jail");
        assert_eq!(
            PermissionGate.check("read", &json!({"path": "src.rs"}), &jail),
            PermissionDecision::Allow
        );
    }

    #[test]
    fn sensitive_files_are_asked() {
        let temp = tempfile::tempdir().expect("temp");
        std::fs::write(temp.path().join(".env"), "TOKEN=x").expect("file");
        let jail = PathJail::new(temp.path()).expect("jail");
        assert!(matches!(
            PermissionGate.check("read", &json!({"path": ".env"}), &jail),
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
            PermissionGate.check("read", &json!({"path": "/etc/passwd"}), &jail),
            PermissionDecision::Deny(_)
        ));
        assert!(matches!(
            PermissionGate.check("read", &json!({"path": ".git/config"}), &jail),
            PermissionDecision::Deny(_)
        ));
        assert!(matches!(
            PermissionGate.check("bash", &json!({}), &jail),
            PermissionDecision::Deny(_)
        ));
    }
}
