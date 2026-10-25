//! Secret detection for commits and the `secret_scan` tool.

use std::path::Path;
use std::sync::OnceLock;

use ignore::WalkBuilder;
use regex::Regex;
use serde_json::{Value, json};

use crate::error::ToolError;
use crate::path_jail::PathJail;
use crate::tools::{Tool, ToolDefinition, ToolResult, cap_output};

const MAX_FILE_BYTES: u64 = 2_000_000;
const MAX_FINDINGS: usize = 100;
const MAX_PATHS: usize = 200;

/// One detected secret. The value is deliberately not retained.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretFinding {
    /// Workspace-relative path.
    pub path: String,
    /// One-based line number.
    pub line: usize,
    /// Human-readable detection kind.
    pub kind: String,
}

/// Scan one text blob for common credentials.
#[must_use]
pub fn scan_text(path: &str, text: &str) -> Vec<SecretFinding> {
    let mut findings = Vec::new();
    for (index, line) in text.lines().enumerate() {
        for (kind, regex) in patterns() {
            if regex.is_match(line) {
                findings.push(SecretFinding {
                    path: path.to_owned(),
                    line: index + 1,
                    kind: (*kind).to_owned(),
                });
                break;
            }
        }
        if findings.len() >= MAX_FINDINGS {
            break;
        }
    }
    findings
}

fn patterns() -> &'static [(&'static str, Regex)] {
    static PATTERNS: OnceLock<Vec<(&'static str, Regex)>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        [
            ("private-key", r"-----BEGIN [A-Z ]*PRIVATE KEY-----"),
            ("openai-key", r"\bsk-[A-Za-z0-9_-]{8,}"),
            ("github-token", r"\b(?:gh[pousr]_[A-Za-z0-9]{12,}|github_pat_[A-Za-z0-9_]{12,})"),
            ("slack-token", r"\bxox[baprs]-[A-Za-z0-9-]{8,}"),
            ("aws-key", r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b"),
            ("google-key", r"\bAIza[0-9A-Za-z_-]{20,}"),
            ("bearer-token", r"(?i)\bBearer\s+[A-Za-z0-9._~+/-]{12,}"),
            (
                "assigned-secret",
                r#"(?i)\b(?:api[_-]?key|secret|token|password|passwd|authorization)\b\s*[:=]\s*['\"]?[A-Za-z0-9_./+=-]{8,}"#,
            ),
        ]
        .into_iter()
        .map(|(kind, pattern)| (kind, Regex::new(pattern).expect("valid secret regex")))
        .collect()
    })
}

/// Scan explicit workspace paths, including directories.
pub fn scan_paths(jail: &PathJail, paths: &[String]) -> Result<Vec<SecretFinding>, ToolError> {
    if paths.len() > MAX_PATHS {
        return Err(ToolError::InvalidArgs(format!(
            "too many paths; maximum is {MAX_PATHS}"
        )));
    }
    let mut findings = Vec::new();
    for requested in paths {
        let path = jail.resolve(requested)?;
        if !path.exists() {
            continue;
        }
        scan_path(jail, &path, &mut findings)?;
        if findings.len() >= MAX_FINDINGS {
            break;
        }
    }
    findings.truncate(MAX_FINDINGS);
    Ok(findings)
}

fn scan_path(
    jail: &PathJail,
    path: &Path,
    findings: &mut Vec<SecretFinding>,
) -> Result<(), ToolError> {
    if path.is_file() {
        scan_file(jail, path, findings)?;
        return Ok(());
    }
    if !path.is_dir() {
        return Ok(());
    }
    let walker = WalkBuilder::new(path)
        .hidden(false)
        .git_ignore(true)
        .filter_entry(|entry| {
            let name = entry.file_name().to_string_lossy();
            !matches!(name.as_ref(), ".git" | "target" | "node_modules" | ".thais")
        })
        .build();
    for entry in walker.flatten() {
        if entry.file_type().is_some_and(|kind| kind.is_file()) {
            scan_file(jail, entry.path(), findings)?;
            if findings.len() >= MAX_FINDINGS {
                break;
            }
        }
    }
    Ok(())
}

fn scan_file(
    jail: &PathJail,
    path: &Path,
    findings: &mut Vec<SecretFinding>,
) -> Result<(), ToolError> {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(_) => return Ok(()),
    };
    if metadata.len() > MAX_FILE_BYTES {
        return Ok(());
    }
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(_) => return Ok(()),
    };
    if bytes.iter().take(8192).any(|byte| *byte == 0) {
        return Ok(());
    }
    let text = String::from_utf8_lossy(&bytes);
    findings.extend(scan_text(&jail.display(path), &text));
    Ok(())
}

/// Scan the staged diff for secrets.
pub fn scan_staged(jail: &PathJail) -> Result<Vec<SecretFinding>, ToolError> {
    let output = crate::git::git_output(jail, &["diff", "--cached", "--unified=0", "--no-color"])?;
    if !output.status.success() {
        return Err(ToolError::Command(format!(
            "git diff --cached failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut findings = Vec::new();
    let mut path = String::from("(staged)");
    let mut line = 0usize;
    for diff_line in text.lines() {
        if let Some(rest) = diff_line.strip_prefix("+++ b/") {
            path = rest.to_owned();
            continue;
        }
        if let Some(rest) = diff_line.strip_prefix("@@ ") {
            if let Some(start) = rest.split_whitespace().nth(1)
                && let Some(number) = start.strip_prefix('+')
                && let Ok(number) = number.split(',').next().unwrap_or(number).parse::<usize>()
            {
                line = number.saturating_sub(1);
            }
            continue;
        }
        if diff_line.starts_with('+') && !diff_line.starts_with("+++") {
            line += 1;
            let content = &diff_line[1..];
            findings.extend(scan_text(&path, content).into_iter().map(|mut finding| {
                finding.line = line;
                finding
            }));
        } else if !diff_line.starts_with('-') {
            line += 1;
        }
        if findings.len() >= MAX_FINDINGS {
            break;
        }
    }
    findings.truncate(MAX_FINDINGS);
    Ok(findings)
}

/// Scan changed files in the working tree.
pub fn scan_changed(
    jail: &PathJail,
    include_untracked: bool,
) -> Result<Vec<SecretFinding>, ToolError> {
    let output = crate::git::git_output(jail, &["status", "--porcelain"])?;
    if !output.status.success() {
        return Ok(Vec::new());
    }
    let mut paths = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if line.len() < 4 {
            continue;
        }
        if !include_untracked && line.starts_with("??") {
            continue;
        }
        let mut path = line[3..].trim();
        if let Some((_, new)) = path.split_once(" -> ") {
            path = new;
        }
        paths.push(path.trim_matches('"').to_owned());
    }
    scan_paths(jail, &paths)
}

/// Render findings without exposing the matching value.
#[must_use]
pub fn render_findings(findings: &[SecretFinding]) -> String {
    if findings.is_empty() {
        return "(no secrets found)".to_owned();
    }
    let mut output = String::new();
    for finding in findings {
        output.push_str(&format!(
            "{}:{}: {}\n",
            finding.path, finding.line, finding.kind
        ));
    }
    if findings.len() >= MAX_FINDINGS {
        output.push_str("[truncated]\n");
    }
    cap_output(output)
}

/// Secret scanner tool.
pub struct SecretScanTool;

impl Tool for SecretScanTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "secret_scan".to_owned(),
            description:
                "Scan workspace files or the staged diff for common credentials. Findings report path, line, and kind; values are never returned."
                    .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "paths": {"type": "array", "items": {"type": "string"}},
                    "staged": {"type": "boolean"},
                    "include_untracked": {"type": "boolean"}
                },
                "additionalProperties": false
            }),
        }
    }

    fn execute(&self, args: Value, jail: &PathJail) -> Result<ToolResult, ToolError> {
        let findings = if args.get("staged").and_then(Value::as_bool).unwrap_or(false) {
            scan_staged(jail)?
        } else {
            let paths = args
                .get("paths")
                .and_then(Value::as_array)
                .map(|paths| {
                    paths
                        .iter()
                        .filter_map(Value::as_str)
                        .map(ToOwned::to_owned)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            if paths.is_empty() {
                scan_changed(
                    jail,
                    args.get("include_untracked")
                        .and_then(Value::as_bool)
                        .unwrap_or(true),
                )?
            } else {
                scan_paths(jail, &paths)?
            }
        };
        Ok(ToolResult {
            content: render_findings(&findings),
            is_error: !findings.is_empty(),
            diff: None,
        })
    }

    fn summary(&self, args: &Value) -> String {
        if args.get("staged").and_then(Value::as_bool).unwrap_or(false) {
            "secret_scan staged".to_owned()
        } else {
            "secret_scan workspace".to_owned()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ToolRegistry;

    #[test]
    fn detects_common_tokens_without_exposing_them() {
        let findings = scan_text(
            "config",
            "key = sk-live-secret-123\nclean = ok\ntoken = ghp_deadbeefdeadbeefdeadbeef\n",
        );
        assert_eq!(findings.len(), 2);
        assert_eq!(findings[0].line, 1);
        assert_eq!(findings[1].line, 3);
        let rendered = render_findings(&findings);
        assert!(!rendered.contains("sk-live"));
        assert!(!rendered.contains("deadbeef"));
        assert!(rendered.contains("openai-key"));
        assert!(rendered.contains("github-token"));
    }

    #[test]
    fn scans_paths_and_staged_diff() {
        let temp = tempfile::tempdir().expect("temp");
        std::fs::write(
            temp.path().join("secret.txt"),
            "password = supersecret123\n",
        )
        .expect("secret");
        std::fs::write(temp.path().join("clean.txt"), "hello\n").expect("clean");
        let registry = ToolRegistry::read_write(temp.path()).expect("registry");
        let result = registry
            .execute("secret_scan", json!({"paths": ["secret.txt"]}))
            .expect("scan");
        assert!(result.is_error);
        assert!(result.content.contains("secret.txt:1"));
        assert!(!result.content.contains("supersecret123"));

        let status = std::process::Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .current_dir(temp.path())
            .status()
            .expect("init");
        assert!(status.success());
        let status = std::process::Command::new("git")
            .args([
                "-c",
                "user.name=test",
                "-c",
                "user.email=test@example.invalid",
                "add",
                "secret.txt",
            ])
            .current_dir(temp.path())
            .status()
            .expect("add");
        assert!(status.success());
        let staged = registry
            .execute("secret_scan", json!({"staged": true}))
            .expect("staged");
        assert!(staged.is_error);
        assert!(staged.content.contains("secret.txt"));
    }

    #[test]
    fn commit_is_blocked_by_secret_scan() {
        let temp = tempfile::tempdir().expect("temp");
        assert!(
            std::process::Command::new("git")
                .args(["init", "-q", "-b", "main"])
                .current_dir(temp.path())
                .status()
                .expect("init")
                .success()
        );
        let registry = ToolRegistry::read_write(temp.path()).expect("registry");
        std::fs::write(temp.path().join("leak.txt"), "sk-live-secret-123\n").expect("leak");
        let error = registry
            .execute(
                "git_commit",
                json!({"message": "add leak", "paths": ["leak.txt"]}),
            )
            .expect_err("blocked");
        assert!(error.to_string().contains("secret"));
        assert!(!error.to_string().contains("sk-live-secret"));
    }

    #[test]
    fn commit_without_paths_scans_untracked_files() {
        let temp = tempfile::tempdir().expect("temp");
        assert!(
            std::process::Command::new("git")
                .args(["init", "-q", "-b", "main"])
                .current_dir(temp.path())
                .status()
                .expect("init")
                .success()
        );
        let registry = ToolRegistry::read_write(temp.path()).expect("registry");
        std::fs::write(
            temp.path().join("leak.txt"),
            "ghp_deadbeefdeadbeefdeadbeef\n",
        )
        .expect("leak");
        let error = registry
            .execute("git_commit", json!({"message": "add leak"}))
            .expect_err("blocked");
        assert!(error.to_string().contains("github-token"));
        assert!(!error.to_string().contains("deadbeef"));
    }
}
