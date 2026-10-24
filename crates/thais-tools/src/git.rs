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

/// Show recent commits.
pub struct GitLogTool;

impl Tool for GitLogTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "git_log".to_owned(),
            description: "Show recent git commits in one-line form.".to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "limit": {"type": "integer", "minimum": 1, "maximum": 100}
                },
                "additionalProperties": false
            }),
        }
    }

    fn execute(&self, args: Value, jail: &PathJail) -> Result<ToolResult, ToolError> {
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(20)
            .clamp(1, 100);
        let limit = format!("-{limit}");
        run_git(jail, &["log", "--oneline", "--decorate", &limit])
    }
}

/// Show one commit.
pub struct GitShowTool;

impl Tool for GitShowTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "git_show".to_owned(),
            description: "Show one commit without invoking external diff helpers.".to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "rev": {"type": "string", "minLength": 1},
                    "stat": {"type": "boolean"}
                },
                "required": ["rev"],
                "additionalProperties": false
            }),
        }
    }

    fn execute(&self, args: Value, jail: &PathJail) -> Result<ToolResult, ToolError> {
        let rev = required_str(&args, "rev")?;
        if !valid_rev(rev) {
            return Err(ToolError::InvalidArgs("invalid git revision".to_owned()));
        }
        let mut git_args = vec!["show", "--no-ext-diff", "--no-textconv"];
        if args.get("stat").and_then(Value::as_bool).unwrap_or(false) {
            git_args.push("--stat");
        }
        git_args.push(rev);
        run_git(jail, &git_args)
    }
}

/// Create one commit. History is never rewritten.
pub struct GitCommitTool;

impl Tool for GitCommitTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "git_commit".to_owned(),
            description: "Stage and commit changes with an explicit message. Does not amend or rewrite history."
                .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "message": {"type": "string", "minLength": 1},
                    "paths": {"type": "array", "items": {"type": "string"}}
                },
                "required": ["message"],
                "additionalProperties": false
            }),
        }
    }

    fn execute(&self, args: Value, jail: &PathJail) -> Result<ToolResult, ToolError> {
        let message = required_str(&args, "message")?;
        if message.contains('\n') || message.trim().is_empty() {
            return Err(ToolError::InvalidArgs(
                "commit message must be one non-empty line".to_owned(),
            ));
        }
        let paths = args
            .get("paths")
            .and_then(Value::as_array)
            .map(|paths| {
                paths
                    .iter()
                    .filter_map(Value::as_str)
                    .map(|path| {
                        jail.resolve(path).map(|resolved| {
                            resolved
                                .strip_prefix(jail.root())
                                .unwrap_or(&resolved)
                                .to_string_lossy()
                                .into_owned()
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()
            })
            .transpose()?
            .unwrap_or_default();

        if paths.is_empty() {
            run_git_checked(jail, &["add", "-A"])?;
        } else {
            let mut add = vec!["add", "-A", "--"];
            add.extend(paths.iter().map(String::as_str));
            run_git_checked(jail, &add)?;
        }
        let mut commit = vec!["commit", "-m", message];
        if !paths.is_empty() {
            commit.push("--");
            commit.extend(paths.iter().map(String::as_str));
        }
        run_git_checked(jail, &commit)
    }

    fn mutating(&self) -> bool {
        true
    }

    fn summary(&self, args: &Value) -> String {
        format!(
            "git_commit {}",
            args.get("message").and_then(Value::as_str).unwrap_or("")
        )
    }
}

fn valid_rev(rev: &str) -> bool {
    !rev.starts_with('-')
        && !rev.contains("..")
        && rev
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "_./@~^-".contains(character))
}

fn required_str<'a>(args: &'a Value, key: &str) -> Result<&'a str, ToolError> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| ToolError::InvalidArgs(format!("missing {key}")))
}

fn run_git_checked(jail: &PathJail, args: &[&str]) -> Result<ToolResult, ToolError> {
    let output = git_output(jail, args)?;
    if !output.status.success() {
        return Err(ToolError::Command(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let mut content = String::from("stdout:\n");
    content.push_str(&String::from_utf8_lossy(&output.stdout));
    content.push_str("\nstderr:\n");
    content.push_str(&String::from_utf8_lossy(&output.stderr));
    Ok(ToolResult::ok(cap_output(content)))
}

fn git_output(jail: &PathJail, args: &[&str]) -> Result<std::process::Output, ToolError> {
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
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_AUTHOR_NAME", "thais")
        .env("GIT_AUTHOR_EMAIL", "thais@localhost")
        .env("GIT_COMMITTER_NAME", "thais")
        .env("GIT_COMMITTER_EMAIL", "thais@localhost");
    command
        .output()
        .map_err(|error| ToolError::Command(format!("could not run git: {error}")))
}

fn run_git(jail: &PathJail, args: &[&str]) -> Result<ToolResult, ToolError> {
    let output = git_output(jail, args)?;
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
    fn log_show_and_commit_work_without_rewriting_history() {
        let (temp, _) = git_repo();
        let registry = ToolRegistry::read_write(temp.path()).expect("registry");
        std::fs::write(temp.path().join("file.txt"), "two\n").expect("file");
        let commit = registry
            .execute(
                "git_commit",
                json!({"message": "change file", "paths": ["file.txt"]}),
            )
            .expect("commit");
        assert!(!commit.is_error);
        let log = registry
            .execute("git_log", json!({"limit": 5}))
            .expect("log");
        assert!(log.content.contains("change file"));
        let show = registry
            .execute("git_show", json!({"rev": "HEAD", "stat": true}))
            .expect("show");
        assert!(show.content.contains("change file"));
    }

    #[test]
    fn git_tools_are_not_mutating() {
        let (_temp, registry) = git_repo();
        assert!(!registry.is_mutating("git_status"));
        assert!(!registry.is_mutating("git_diff"));
    }
}
