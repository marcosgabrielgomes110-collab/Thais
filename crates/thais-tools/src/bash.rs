//! Shell execution with a timeout, environment scrubbing, and output caps.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::{Value, json};
use wait_timeout::ChildExt;

use crate::error::ToolError;
use crate::path_jail::PathJail;
use crate::sandbox::Sandbox;
use crate::tools::{Tool, ToolDefinition, ToolResult, cap_output, metadata, required_str};

const DEFAULT_TIMEOUT_MS: u64 = 30_000;
const MAX_TIMEOUT_MS: u64 = 120_000;
const MAX_STREAM_BYTES: usize = 100_000;

/// Execute a shell command in the workspace. The gate must ask before this
/// tool can run.
pub struct BashTool {
    sandbox: Sandbox,
}

impl BashTool {
    /// Build a bash tool with auto-detected sandbox settings.
    #[must_use]
    pub fn new() -> Self {
        Self {
            sandbox: Sandbox::from_env(),
        }
    }

    /// Build a bash tool with an explicit sandbox policy.
    #[must_use]
    pub fn with_sandbox(sandbox: Sandbox) -> Self {
        Self { sandbox }
    }
}

impl Default for BashTool {
    fn default() -> Self {
        Self::new()
    }
}

impl Tool for BashTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "bash".to_owned(),
            description:
                "Run a shell command in the workspace. Every call requires approval. A best-effort OS sandbox is used when available."
                    .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string"},
                    "cwd": {"type": "string"},
                    "timeout_ms": {"type": "integer", "minimum": 100, "maximum": 120000}
                },
                "required": ["command"],
                "additionalProperties": false
            }),
        }
    }

    fn mutating(&self) -> bool {
        true
    }

    fn summary(&self, args: &Value) -> String {
        let command = args.get("command").and_then(Value::as_str).unwrap_or("");
        format!("bash: {}", truncate(command, 200))
    }

    fn execute(&self, args: Value, jail: &PathJail) -> Result<ToolResult, ToolError> {
        let command = required_str(&args, "command")?;
        let cwd = jail.resolve(args.get("cwd").and_then(Value::as_str).unwrap_or("."))?;
        if !metadata(&cwd)?.is_dir() {
            return Err(ToolError::InvalidArgs(format!(
                "{} is not a directory",
                jail.display(&cwd)
            )));
        }
        let timeout_ms = args
            .get("timeout_ms")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_TIMEOUT_MS)
            .clamp(100, MAX_TIMEOUT_MS);

        let mut process = self
            .sandbox
            .command(command, jail.root(), &cwd)
            .map_err(ToolError::Command)?;
        process
            .current_dir(&cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        scrub_environment(&mut process);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            process.process_group(0);
        }

        let mut child = process
            .spawn()
            .map_err(|error| ToolError::Command(format!("could not spawn command: {error}")))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ToolError::Command("stdout pipe unavailable".to_owned()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| ToolError::Command("stderr pipe unavailable".to_owned()))?;
        let stdout_reader = std::thread::spawn(move || read_capped(stdout));
        let stderr_reader = std::thread::spawn(move || read_capped(stderr));

        let status = child
            .wait_timeout(Duration::from_millis(timeout_ms))
            .map_err(|error| ToolError::Command(format!("wait failed: {error}")))?;
        let timed_out = status.is_none();
        if timed_out {
            kill_process_group(&mut child);
            let _ = child.wait();
        }

        let (stdout, stdout_truncated) = stdout_reader
            .join()
            .map_err(|_| ToolError::Command("stdout reader panicked".to_owned()))?
            .map_err(|source| ToolError::Io {
                path: cwd.clone(),
                source,
            })?;
        let (stderr, stderr_truncated) = stderr_reader
            .join()
            .map_err(|_| ToolError::Command("stderr reader panicked".to_owned()))?
            .map_err(|source| ToolError::Io {
                path: cwd.clone(),
                source,
            })?;

        let exit_code = status.and_then(|status| status.code()).unwrap_or(-1);
        let mut content = format!("exit_code: {exit_code}\n");
        if timed_out {
            content.push_str(&format!("timed_out: true ({timeout_ms} ms)\n"));
        }
        content.push_str("stdout:\n");
        content.push_str(&stdout);
        if stdout_truncated {
            content.push_str("\n[stdout truncated]");
        }
        content.push_str("\nstderr:\n");
        content.push_str(&stderr);
        if stderr_truncated {
            content.push_str("\n[stderr truncated]");
        }

        Ok(ToolResult::ok(cap_output(content)))
    }
}

pub(crate) fn scrub_environment(process: &mut Command) {
    process.env_clear();
    #[cfg(unix)]
    let allow = [
        "PATH", "HOME", "USER", "LOGNAME", "SHELL", "LANG", "LC_ALL", "TERM", "TMPDIR", "TZ",
    ];
    #[cfg(windows)]
    let allow = [
        "PATH",
        "USERPROFILE",
        "HOMEDRIVE",
        "HOMEPATH",
        "SystemRoot",
        "COMSPEC",
        "PATHEXT",
        "TEMP",
        "TMP",
        "WINDIR",
    ];
    for key in allow {
        if let Some(value) = std::env::var_os(key) {
            process.env(key, value);
        }
    }
}

fn read_capped(mut reader: impl Read) -> std::io::Result<(String, bool)> {
    let mut output = Vec::new();
    let mut truncated = false;
    let mut buffer = [0_u8; 8192];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        if output.len() < MAX_STREAM_BYTES {
            let remaining = MAX_STREAM_BYTES - output.len();
            let take = remaining.min(read);
            output.extend_from_slice(&buffer[..take]);
            if take < read {
                truncated = true;
            }
        } else {
            truncated = true;
        }
    }
    Ok((String::from_utf8_lossy(&output).into_owned(), truncated))
}

fn kill_process_group(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        let _ = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(child.id() as i32),
            nix::sys::signal::Signal::SIGKILL,
        );
    }
    let _ = child.kill();
}

fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        text.to_owned()
    } else {
        format!("{}…", text.chars().take(max_chars).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ToolRegistry;

    #[test]
    fn bash_runs_a_command_and_returns_output() {
        let temp = tempfile::tempdir().expect("temp");
        let registry = ToolRegistry::read_write(temp.path()).expect("registry");
        let result = registry
            .execute("bash", json!({"command": "printf hello"}))
            .expect("bash");
        assert!(!result.is_error);
        assert!(result.content.contains("exit_code: 0"));
        assert!(result.content.contains("hello"));
        assert!(registry.is_mutating("bash"));
        assert!(
            registry
                .summary("bash", &json!({"command": "ls"}))
                .contains("bash: ls")
        );
    }

    #[cfg(unix)]
    #[test]
    fn bash_times_out_and_kills_the_child() {
        let temp = tempfile::tempdir().expect("temp");
        let registry = ToolRegistry::read_write(temp.path()).expect("registry");
        let result = registry
            .execute("bash", json!({"command": "sleep 2", "timeout_ms": 100}))
            .expect("bash");
        assert!(result.content.contains("timed_out: true"));
    }

    #[test]
    fn scrubbed_environment_only_contains_the_allowlist() {
        let mut process = Command::new("sh");
        scrub_environment(&mut process);
        #[cfg(unix)]
        let allowed = [
            "PATH", "HOME", "USER", "LOGNAME", "SHELL", "LANG", "LC_ALL", "TERM", "TMPDIR", "TZ",
        ];
        #[cfg(windows)]
        let allowed = [
            "PATH",
            "USERPROFILE",
            "HOMEDRIVE",
            "HOMEPATH",
            "SystemRoot",
            "COMSPEC",
            "PATHEXT",
            "TEMP",
            "TMP",
            "WINDIR",
        ];
        for (key, _) in process.get_envs() {
            let key = key.to_string_lossy();
            assert!(allowed.contains(&key.as_ref()), "unexpected env var: {key}");
        }
    }

    #[test]
    fn bash_rejects_a_cwd_escape() {
        let temp = tempfile::tempdir().expect("temp");
        let registry = ToolRegistry::read_write(temp.path()).expect("registry");
        let result = registry.execute("bash", json!({"command": "true", "cwd": "../outside"}));
        assert!(matches!(result, Err(ToolError::Escape(_))));
    }
}
