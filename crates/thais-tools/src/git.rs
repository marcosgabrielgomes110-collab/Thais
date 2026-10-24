//! Read-only Git tools.

use std::process::Command;

use serde_json::{Value, json};

use crate::error::ToolError;
use crate::path_jail::PathJail;
use crate::tools::{Tool, ToolDefinition, ToolResult, cap_output};

/// Show the working tree status.
pub struct GitStatusTool;

impl Tool for GitStatusTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "git_status".to_owned(),
            description: "Show git status --short for the workspace.".to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        }
    }

    fn execute(&self, _args: Value, jail: &PathJail) -> Result<ToolResult, ToolError> {
        run_git(jail, &["status", "--short"])
    }
}

/// Show a git diff.
pub struct GitDiffTool;

impl Tool for GitDiffTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "git_diff".to_owned(),
            description: "Show a git diff, optionally for one path or the staged index.".to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "staged": {"type": "boolean"}
                },
                "additionalProperties": false
            }),
        }
    }

    fn execute(&self, args: Value, jail: &PathJail) -> Result<ToolResult, ToolError> {
        let staged = args.get("staged").and_then(Value::as_bool).unwrap_or(false);
        let mut git_args: Vec<String> = vec!["diff".to_owned()];
        if staged {
            git_args.push("--cached".to_owned());
        }
        if let Some(path) = args.get("path").and_then(Value::as_str) {
            let path = jail.resolve(path)?;
            let relative = path.strip_prefix(jail.root()).unwrap_or(&path);
            git_args.push("--".to_owned());
            git_args.push(relative.to_string_lossy().into_owned());
        }
        let args = git_args.iter().map(String::as_str).collect::<Vec<_>>();
        run_git(jail, &args)
    }
}

fn run_git(jail: &PathJail, args: &[&str]) -> Result<ToolResult, ToolError> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(jail.root())
        .args(args)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env(
            "HOME",
            std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .unwrap_or_default(),
        )
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0");
    let output = command
        .output()
        .map_err(|error| ToolError::Command(format!("could not run git: {error}")))?;
    let mut content = format!("exit_code: {}\n", output.status.code().unwrap_or(-1));
    content.push_str("stdout:\n");
    content.push_str(&String::from_utf8_lossy(&output.stdout));
    content.push_str("\nstderr:\n");
    content.push_str(&String::from_utf8_lossy(&output.stderr));
    Ok(ToolResult::ok(cap_output(content)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ToolRegistry;

    fn git_repo() -> (tempfile::TempDir, ToolRegistry) {
        let temp = tempfile::tempdir().expect("temp");
        let status = Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .current_dir(temp.path())
            .status()
            .expect("git init");
        assert!(status.success());
        std::fs::write(temp.path().join("file.txt"), "one\n").expect("file");
        let status = Command::new("git")
            .args([
                "-c",
                "user.name=test",
                "-c",
                "user.email=test@example.invalid",
                "add",
                ".",
            ])
            .current_dir(temp.path())
            .status()
            .expect("git add");
        assert!(status.success());
        let status = Command::new("git")
            .args([
                "-c",
                "user.name=test",
                "-c",
                "user.email=test@example.invalid",
                "commit",
                "-qm",
                "init",
            ])
            .current_dir(temp.path())
            .status()
            .expect("git commit");
        assert!(status.success());
        let registry = ToolRegistry::read_only(temp.path()).expect("registry");
        (temp, registry)
    }

    #[test]
    fn status_and_diff_report_working_tree_changes() {
        let (temp, registry) = git_repo();
        std::fs::write(temp.path().join("file.txt"), "two\n").expect("file");

        let status = registry.execute("git_status", json!({})).expect("status");
        assert!(status.content.contains("file.txt"));

        let diff = registry
            .execute("git_diff", json!({"path": "file.txt"}))
            .expect("diff");
        assert!(diff.content.contains("-one"));
        assert!(diff.content.contains("+two"));
    }

    #[test]
    fn git_tools_are_not_mutating() {
        let (_temp, registry) = git_repo();
        assert!(!registry.is_mutating("git_status"));
        assert!(!registry.is_mutating("git_diff"));
    }
}
