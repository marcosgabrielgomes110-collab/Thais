//! Structured project build and test execution.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use regex::Regex;
use serde_json::{Value, json};
use wait_timeout::ChildExt;

use crate::bash::scrub_environment;
use crate::error::ToolError;
use crate::path_jail::PathJail;
use crate::sandbox::Sandbox;
use crate::tools::{Approval, Tool, ToolDefinition, ToolResult, cap_output, metadata};

const MAX_OUTPUT_BYTES: usize = 2_000_000;
const MAX_REPORT_LOCATIONS: usize = 50;
const MAX_TAIL_CHARS: usize = 2_000;

struct CommandResult {
    exit_code: Option<i32>,
    timed_out: bool,
    duration_ms: u128,
    text: String,
}

fn run(
    command: &str,
    cwd: &Path,
    workspace: &Path,
    sandbox: &Sandbox,
    timeout: Duration,
) -> Result<CommandResult, ToolError> {
    let mut process = sandbox
        .command(command, workspace, cwd)
        .map_err(ToolError::Command)?;
    process
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    scrub_environment(&mut process);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        process.process_group(0);
    }
    let started = Instant::now();
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
        .wait_timeout(timeout)
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
            path: cwd.to_path_buf(),
            source,
        })?;
    let (stderr, stderr_truncated) = stderr_reader
        .join()
        .map_err(|_| ToolError::Command("stderr reader panicked".to_owned()))?
        .map_err(|source| ToolError::Io {
            path: cwd.to_path_buf(),
            source,
        })?;
    let mut text = String::from_utf8_lossy(&stdout).into_owned();
    if !stderr.is_empty() {
        if !text.ends_with('\n') && !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&String::from_utf8_lossy(&stderr));
    }
    if stdout_truncated || stderr_truncated {
        text.push_str("\n[output truncated at 2 MB]\n");
    }
    Ok(CommandResult {
        exit_code: status.and_then(|status| status.code()),
        timed_out,
        duration_ms: started.elapsed().as_millis(),
        text,
    })
}

fn read_capped(mut reader: impl Read) -> std::io::Result<(Vec<u8>, bool)> {
    let mut output = Vec::new();
    let mut truncated = false;
    let mut buffer = [0_u8; 8192];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        if output.len() < MAX_OUTPUT_BYTES {
            let take = (MAX_OUTPUT_BYTES - output.len()).min(read);
            output.extend_from_slice(&buffer[..take]);
            if take < read {
                truncated = true;
            }
        } else {
            truncated = true;
        }
    }
    Ok((output, truncated))
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

fn validate_package(package: Option<&str>) -> Result<Option<String>, ToolError> {
    let Some(package) = package else {
        return Ok(None);
    };
    if package.is_empty()
        || package.len() > 128
        || !package
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "_-".contains(character))
    {
        return Err(ToolError::InvalidArgs("invalid package name".to_owned()));
    }
    Ok(Some(package.to_owned()))
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn timeout_ms(args: &Value) -> u64 {
    args.get("timeout_ms")
        .and_then(Value::as_u64)
        .unwrap_or(300_000)
        .clamp(100, 300_000)
}

fn write_log(dir: &Path, prefix: &str, text: &str) -> Result<PathBuf, ToolError> {
    std::fs::create_dir_all(dir).map_err(|source| ToolError::Io {
        path: dir.to_path_buf(),
        source,
    })?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis());
    let path = dir.join(format!("{prefix}-{stamp}.log"));
    std::fs::write(&path, text).map_err(|source| ToolError::Io {
        path: path.clone(),
        source,
    })?;
    Ok(path)
}

fn failure_locations(text: &str) -> Vec<String> {
    static LOCATION: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let regex = LOCATION.get_or_init(|| {
        Regex::new(
            r"(?m)(?:^\s*-->\s+|panicked at |)([A-Za-z0-9_./\\-]+\.(?:rs|toml|py|js|ts|tsx|go|c|cc|cpp|h)):(\d+)(?::(\d+))?",
        )
        .expect("valid location regex")
    });
    let mut locations = Vec::new();
    for captures in regex.captures_iter(text) {
        let path = captures.get(1).map_or("", |value| value.as_str());
        let line = captures.get(2).map_or("", |value| value.as_str());
        let location = if let Some(column) = captures.get(3) {
            format!("{path}:{line}:{}", column.as_str())
        } else {
            format!("{path}:{line}")
        };
        if !locations.contains(&location) {
            locations.push(location);
            if locations.len() >= MAX_REPORT_LOCATIONS {
                break;
            }
        }
    }
    locations
}

fn count_test_failures(text: &str) -> usize {
    text.lines()
        .filter(|line| {
            let line = line.trim();
            line.starts_with("test ") && line.ends_with(" FAILED")
        })
        .count()
}

fn count_build_errors(text: &str) -> usize {
    text.lines()
        .filter(|line| {
            let line = line.trim();
            line.starts_with("error[") || line.starts_with("error:")
        })
        .count()
}

fn tail(text: &str) -> String {
    if text.chars().count() <= MAX_TAIL_CHARS {
        return text.to_owned();
    }
    let tail = text
        .chars()
        .rev()
        .take(MAX_TAIL_CHARS)
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>();
    format!("…{tail}")
}

fn render(
    result: &CommandResult,
    failures: usize,
    locations: &[String],
    log_path: &Path,
    workspace: &Path,
) -> String {
    let mut output = format!(
        "status: {}\nexit_code: {}\nduration_ms: {}\nfailures: {}\n",
        if result.timed_out {
            "timed_out"
        } else if result.exit_code == Some(0) {
            "passed"
        } else {
            "failed"
        },
        result
            .exit_code
            .map_or_else(|| "none".to_owned(), |code| code.to_string()),
        result.duration_ms,
        failures
    );
    if !locations.is_empty() {
        output.push_str("locations:\n");
        for location in locations.iter().take(MAX_REPORT_LOCATIONS) {
            output.push_str(location);
            output.push('\n');
        }
    }
    output.push_str("log: ");
    output.push_str(
        &log_path
            .strip_prefix(workspace)
            .unwrap_or(log_path)
            .display()
            .to_string(),
    );
    output.push('\n');
    output.push_str("tail:\n");
    output.push_str(&tail(&result.text));
    cap_output(output)
}

fn command_for_test(args: &Value) -> Result<(String, Option<String>), ToolError> {
    let package = validate_package(args.get("package").and_then(Value::as_str))?;
    let filter = args
        .get("filter")
        .and_then(Value::as_str)
        .filter(|filter| !filter.is_empty());
    let mut command = "cargo test --no-fail-fast".to_owned();
    if let Some(package) = &package {
        command.push_str(" --package ");
        command.push_str(package);
    }
    if let Some(filter) = filter {
        if filter.chars().count() > 256 {
            return Err(ToolError::InvalidArgs("filter is too long".to_owned()));
        }
        command.push_str(" -- ");
        command.push_str(&shell_quote(filter));
    }
    Ok((command, package))
}

fn command_for_build(args: &Value) -> Result<(String, Option<String>), ToolError> {
    let package = validate_package(args.get("package").and_then(Value::as_str))?;
    let mut command = "cargo build".to_owned();
    if args
        .get("release")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        command.push_str(" --release");
    }
    if let Some(package) = &package {
        command.push_str(" --package ");
        command.push_str(package);
    }
    Ok((command, package))
}

/// Structured Cargo test tool.
pub struct TestTool {
    sandbox: Sandbox,
    log_dir: PathBuf,
}

impl TestTool {
    /// Build a test tool.
    #[must_use]
    pub fn new(sandbox: Sandbox, log_dir: PathBuf) -> Self {
        Self { sandbox, log_dir }
    }
}

impl Tool for TestTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "test".to_owned(),
            description:
                "Run cargo test for this workspace. Returns a compact failure summary and a path to the full log. Requires approval and does not create a snapshot."
                    .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "package": {"type": "string"},
                    "filter": {"type": "string"},
                    "timeout_ms": {"type": "integer", "minimum": 100, "maximum": 300000}
                },
                "additionalProperties": false
            }),
        }
    }

    fn approval(&self) -> Approval {
        Approval::Ask
    }

    fn summary(&self, args: &Value) -> String {
        format!(
            "test{}",
            args.get("package")
                .and_then(Value::as_str)
                .map_or_else(String::new, |package| format!(" {package}"))
        )
    }

    fn execute(&self, args: Value, jail: &PathJail) -> Result<ToolResult, ToolError> {
        if !metadata(jail.root())?.is_dir() || !jail.root().join("Cargo.toml").is_file() {
            return Err(ToolError::InvalidArgs(
                "test is only supported for Cargo workspaces".to_owned(),
            ));
        }
        let (command, _package) = command_for_test(&args)?;
        let timeout = Duration::from_millis(timeout_ms(&args));
        let result = run(&command, jail.root(), jail.root(), &self.sandbox, timeout)?;
        let failures = count_test_failures(&result.text);
        let locations = failure_locations(&result.text);
        let log_path = write_log(&self.log_dir, "test", &result.text)?;
        let content = render(&result, failures, &locations, &log_path, jail.root());
        Ok(ToolResult {
            content,
            is_error: result.timed_out || result.exit_code != Some(0),
            diff: None,
        })
    }
}

/// Structured Cargo build tool.
pub struct BuildTool {
    sandbox: Sandbox,
    log_dir: PathBuf,
}

impl BuildTool {
    /// Build a build tool.
    #[must_use]
    pub fn new(sandbox: Sandbox, log_dir: PathBuf) -> Self {
        Self { sandbox, log_dir }
    }
}

impl Tool for BuildTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "build".to_owned(),
            description:
                "Run cargo build for this workspace. Returns compact compiler locations and a path to the full log. Requires approval and does not create a snapshot."
                    .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "package": {"type": "string"},
                    "release": {"type": "boolean"},
                    "timeout_ms": {"type": "integer", "minimum": 100, "maximum": 300000}
                },
                "additionalProperties": false
            }),
        }
    }

    fn approval(&self) -> Approval {
        Approval::Ask
    }

    fn summary(&self, args: &Value) -> String {
        format!(
            "build{}",
            args.get("package")
                .and_then(Value::as_str)
                .map_or_else(String::new, |package| format!(" {package}"))
        )
    }

    fn execute(&self, args: Value, jail: &PathJail) -> Result<ToolResult, ToolError> {
        if !metadata(jail.root())?.is_dir() || !jail.root().join("Cargo.toml").is_file() {
            return Err(ToolError::InvalidArgs(
                "build is only supported for Cargo workspaces".to_owned(),
            ));
        }
        let (command, _package) = command_for_build(&args)?;
        let timeout = Duration::from_millis(timeout_ms(&args));
        let result = run(&command, jail.root(), jail.root(), &self.sandbox, timeout)?;
        let failures = count_build_errors(&result.text);
        let locations = failure_locations(&result.text);
        let log_path = write_log(&self.log_dir, "build", &result.text)?;
        let content = render(&result, failures, &locations, &log_path, jail.root());
        Ok(ToolResult {
            content,
            is_error: result.timed_out || result.exit_code != Some(0),
            diff: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::{Sandbox, SandboxMode};
    use crate::tools::ToolRegistry;

    #[test]
    fn parses_test_and_build_failures() {
        let test_output = "test result: FAILED. 0 passed\nthread 'x' panicked at src/lib.rs:4:5\n";
        assert_eq!(count_test_failures("test foo ... FAILED\n"), 1);
        assert_eq!(failure_locations(test_output), ["src/lib.rs:4:5"]);

        let build_output = "error[E0308]: mismatched types\n --> src/main.rs:7:13\n";
        assert_eq!(count_build_errors(build_output), 1);
        assert_eq!(failure_locations(build_output), ["src/main.rs:7:13"]);
    }

    fn cargo_project(temp: &tempfile::TempDir) {
        std::fs::write(
            temp.path().join("Cargo.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .expect("manifest");
        std::fs::create_dir_all(temp.path().join("src")).expect("src");
    }

    fn registry(temp: &tempfile::TempDir) -> ToolRegistry {
        ToolRegistry::read_write(temp.path()).expect("registry")
    }

    #[test]
    fn build_failure_reports_file_and_line() {
        let temp = tempfile::tempdir().expect("temp");
        cargo_project(&temp);
        std::fs::write(
            temp.path().join("src/main.rs"),
            "fn main() { let _x: i32 = \"wrong\"; }\n",
        )
        .expect("main");
        let result = registry(&temp)
            .execute("build", json!({"timeout_ms": 60_000}))
            .expect("build");
        assert!(result.is_error);
        assert!(
            result.content.contains("src/main.rs:"),
            "{}",
            result.content
        );
        assert!(result.content.contains("log: target/thais-logs/"));
        assert!(!result.content.contains("warning: unused"));
    }

    #[test]
    fn test_failure_reports_file_and_line() {
        let temp = tempfile::tempdir().expect("temp");
        cargo_project(&temp);
        std::fs::write(
            temp.path().join("src/lib.rs"),
            "#[test]\nfn broken() { assert!(false); }\n",
        )
        .expect("lib");
        let result = registry(&temp)
            .execute("test", json!({"timeout_ms": 60_000}))
            .expect("test");
        assert!(result.is_error);
        assert!(result.content.contains("src/lib.rs:"), "{}", result.content);
        assert!(result.content.contains("log: target/thais-logs/"));
    }

    #[test]
    fn test_and_build_ask_without_mutating() {
        let temp = tempfile::tempdir().expect("temp");
        let registry = registry(&temp);
        for name in ["test", "build"] {
            assert!(registry.is_known(name));
            assert!(!registry.is_mutating(name));
            assert_eq!(registry.approval(name), Approval::Ask);
        }
        let _ = Sandbox::detect(SandboxMode::Off);
    }
}
