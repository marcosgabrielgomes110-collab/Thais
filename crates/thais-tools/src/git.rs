//! Read-only Git tools.

use std::process::Command;

use serde_json::{Value, json};

use crate::error::ToolError;
use crate::path_jail::PathJail;
use crate::secret::{render_findings, scan_changed, scan_paths};
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

        let mut secret_findings = if paths.is_empty() {
            scan_changed(jail, true)?
        } else {
            scan_paths(jail, &paths)?
        };
        secret_findings.extend(crate::secret::scan_staged(jail)?);
        secret_findings.sort_by(|left, right| {
            (&left.path, left.line, &left.kind).cmp(&(&right.path, right.line, &right.kind))
        });
        secret_findings.dedup();
        if !secret_findings.is_empty() {
            return Err(ToolError::InvalidArgs(format!(
                "secret scan blocked commit:\n{}",
                render_findings(&secret_findings)
            )));
        }

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

pub(crate) fn git_output(
    jail: &PathJail,
    args: &[&str],
) -> Result<std::process::Output, ToolError> {
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

/// List local and remote branches.
pub struct GitBranchTool;

impl Tool for GitBranchTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "git_branch".to_owned(),
            description: "List git branches without changing the working tree.".to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "all": {"type": "boolean"},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 100}
                },
                "additionalProperties": false
            }),
        }
    }

    fn execute(&self, args: Value, jail: &PathJail) -> Result<ToolResult, ToolError> {
        let mut git_args = vec!["branch", "--no-color"];
        if args.get("all").and_then(Value::as_bool).unwrap_or(false) {
            git_args.push("--all");
        }
        let result = run_git(jail, &git_args)?;
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(50)
            .clamp(1, 100) as usize;
        let mut lines = result.content.lines().take(limit + 2).collect::<Vec<_>>();
        if lines.len() > limit + 1 {
            lines.truncate(limit + 1);
            lines.push("[truncated]");
        }
        Ok(ToolResult::ok(cap_output(lines.join("\n"))))
    }
}

/// Switch branches or create a branch without destructive reset operations.
pub struct GitCheckoutTool;

impl Tool for GitCheckoutTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "git_checkout".to_owned(),
            description:
                "Switch to a branch, or create it with create=true. Refuses a dirty worktree and never resets or discards changes."
                    .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "branch": {"type": "string", "minLength": 1},
                    "create": {"type": "boolean"},
                    "start_point": {"type": "string"}
                },
                "required": ["branch"],
                "additionalProperties": false
            }),
        }
    }

    fn mutating(&self) -> bool {
        true
    }

    fn summary(&self, args: &Value) -> String {
        format!(
            "git_checkout {}",
            args.get("branch").and_then(Value::as_str).unwrap_or("")
        )
    }

    fn preview(&self, args: &Value, _jail: &PathJail) -> Option<String> {
        let branch = args.get("branch")?.as_str()?;
        let create = args.get("create").and_then(Value::as_bool).unwrap_or(false);
        Some(format!(
            "git_checkout {}{}\n",
            if create { "create " } else { "" },
            branch
        ))
    }

    fn execute(&self, args: Value, jail: &PathJail) -> Result<ToolResult, ToolError> {
        let branch = required_str(&args, "branch")?;
        if !valid_branch(branch) {
            return Err(ToolError::InvalidArgs("invalid branch name".to_owned()));
        }
        let status = git_output(jail, &["status", "--porcelain"])?;
        if !status.status.success() {
            return Err(ToolError::Command(
                "could not inspect the working tree".to_owned(),
            ));
        }
        if !String::from_utf8_lossy(&status.stdout).trim().is_empty() {
            return Err(ToolError::InvalidArgs(
                "working tree is dirty; commit or stash changes first".to_owned(),
            ));
        }
        let create = args.get("create").and_then(Value::as_bool).unwrap_or(false);
        if create {
            let mut command = vec!["switch", "-c", branch];
            if let Some(start) = args.get("start_point").and_then(Value::as_str) {
                if !valid_rev(start) {
                    return Err(ToolError::InvalidArgs("invalid start_point".to_owned()));
                }
                command.push(start);
            }
            run_git_checked(jail, &command)
        } else {
            if args.get("start_point").is_some() {
                return Err(ToolError::InvalidArgs(
                    "start_point requires create=true".to_owned(),
                ));
            }
            run_git_checked(jail, &["switch", branch])
        }
    }
}

/// Push, pop, or drop a stash entry.
pub struct GitStashTool;

impl Tool for GitStashTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "git_stash".to_owned(),
            description: "Push, pop, or drop a git stash entry. Does not clear the stash list."
                .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["push", "pop", "drop"]},
                    "message": {"type": "string", "maxLength": 200},
                    "index": {"type": "integer", "minimum": 0, "maximum": 20},
                    "include_untracked": {"type": "boolean"}
                },
                "required": ["action"],
                "additionalProperties": false
            }),
        }
    }

    fn mutating(&self) -> bool {
        true
    }

    fn summary(&self, args: &Value) -> String {
        format!(
            "git_stash {}",
            args.get("action").and_then(Value::as_str).unwrap_or("")
        )
    }

    fn preview(&self, args: &Value, _jail: &PathJail) -> Option<String> {
        let action = args.get("action")?.as_str()?;
        let index = args.get("index").and_then(Value::as_u64).unwrap_or(0);
        Some(format!("git_stash {action} stash@{{{index}}}\n"))
    }

    fn execute(&self, args: Value, jail: &PathJail) -> Result<ToolResult, ToolError> {
        let action = required_str(&args, "action")?;
        let index = args.get("index").and_then(Value::as_u64).unwrap_or(0);
        if index > 20 {
            return Err(ToolError::InvalidArgs(
                "stash index is too large".to_owned(),
            ));
        }
        match action {
            "push" => {
                let mut command = vec!["stash", "push"];
                if args
                    .get("include_untracked")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                {
                    command.push("--include-untracked");
                }
                let message = args.get("message").and_then(Value::as_str);
                if let Some(message) = message {
                    if message.contains('\n') || message.chars().count() > 200 {
                        return Err(ToolError::InvalidArgs(
                            "stash message must be one line under 200 chars".to_owned(),
                        ));
                    }
                    command.push("-m");
                    command.push(message);
                }
                run_git_checked(jail, &command)
            }
            "pop" | "drop" => {
                let stash = format!("stash@{{{index}}}");
                run_git_checked(jail, &["stash", action, &stash])
            }
            other => Err(ToolError::InvalidArgs(format!(
                "unknown stash action: {other}"
            ))),
        }
    }
}

fn valid_branch(branch: &str) -> bool {
    !branch.is_empty()
        && branch.len() <= 200
        && !branch.starts_with('-')
        && !branch.contains("..")
        && !branch.contains("@{")
        && !branch.ends_with(".lock")
        && !branch.contains("//")
        && branch
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "_-./".contains(character))
}

#[cfg(test)]
mod workflow_tests {
    use super::*;
    use crate::tools::ToolRegistry;

    fn repo() -> (tempfile::TempDir, ToolRegistry) {
        let temp = tempfile::tempdir().expect("temp");
        assert!(
            Command::new("git")
                .args(["init", "-q", "-b", "main"])
                .current_dir(temp.path())
                .status()
                .expect("init")
                .success()
        );
        std::fs::write(temp.path().join("file.txt"), "one\n").expect("file");
        assert!(
            Command::new("git")
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
                .expect("add")
                .success()
        );
        assert!(
            Command::new("git")
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
                .expect("commit")
                .success()
        );
        let registry = ToolRegistry::read_write(temp.path()).expect("registry");
        (temp, registry)
    }

    #[test]
    fn lists_and_creates_branches() {
        let (_temp, registry) = repo();
        let branches = registry.execute("git_branch", json!({})).expect("branch");
        assert!(branches.content.contains("main"));
        let checkout = registry
            .execute("git_checkout", json!({"branch": "feature", "create": true}))
            .expect("checkout");
        assert!(!checkout.is_error);
        let branches = registry.execute("git_branch", json!({})).expect("branch");
        assert!(branches.content.contains("feature"));
        assert!(!registry.is_mutating("git_branch"));
        assert!(registry.is_mutating("git_checkout"));
    }

    #[test]
    fn checkout_refuses_a_dirty_worktree_and_invalid_names() {
        let (temp, registry) = repo();
        std::fs::write(temp.path().join("file.txt"), "dirty\n").expect("dirty");
        let error = registry
            .execute("git_checkout", json!({"branch": "feature", "create": true}))
            .expect_err("dirty");
        assert!(error.to_string().contains("dirty"));
        let error = registry
            .execute("git_checkout", json!({"branch": "--help"}))
            .expect_err("invalid");
        assert!(error.to_string().contains("invalid branch"));
    }

    #[test]
    fn stash_push_and_pop_round_trip() {
        let (temp, registry) = repo();
        std::fs::write(temp.path().join("file.txt"), "two\n").expect("change");
        let push = registry
            .execute("git_stash", json!({"action": "push", "message": "work"}))
            .expect("push");
        assert!(!push.is_error);
        assert_eq!(
            std::fs::read_to_string(temp.path().join("file.txt")).expect("clean"),
            "one\n"
        );
        let pop = registry
            .execute("git_stash", json!({"action": "pop"}))
            .expect("pop");
        assert!(!pop.is_error);
        assert_eq!(
            std::fs::read_to_string(temp.path().join("file.txt")).expect("restored"),
            "two\n"
        );
        assert!(registry.is_mutating("git_stash"));
    }

    #[test]
    fn stash_rejects_unknown_actions() {
        let (_temp, registry) = repo();
        let error = registry
            .execute("git_stash", json!({"action": "clear"}))
            .expect_err("unknown");
        assert!(error.to_string().contains("unknown stash action"));
    }
}
