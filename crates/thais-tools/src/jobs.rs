//! Background shell jobs with bounded logs and process-group cleanup.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use serde_json::{Value, json};

use crate::bash::scrub_environment;
use crate::error::ToolError;
use crate::path_jail::PathJail;
use crate::sandbox::Sandbox;
use crate::tools::{
    Approval, Tool, ToolDefinition, ToolResult, cap_output, metadata, required_str,
};

const MAX_JOBS: usize = 4;
const MAX_LOG_BYTES: usize = 2_000_000;
const MAX_LOG_READ_BYTES: usize = 64_000;
const DEFAULT_LOG_READ_BYTES: usize = 16_000;
const DEFAULT_JOB_TIMEOUT: Duration = Duration::from_secs(3_600);
const MAX_JOB_TIMEOUT: Duration = Duration::from_secs(3_600);
const ORPHAN_LOG_AGE: Duration = Duration::from_secs(3_600);

/// State of one background job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobStatus {
    /// Process is still running.
    Running,
    /// Process exited with an optional code.
    Exited(Option<i32>),
    /// Process was stopped by the user.
    Stopped,
    /// Process was killed after its timeout.
    TimedOut,
}

impl JobStatus {
    /// Stable status text.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Exited(_) => "exited",
            Self::Stopped => "stopped",
            Self::TimedOut => "timed_out",
        }
    }
}

/// Public snapshot of a job.
#[derive(Debug, Clone)]
pub struct JobInfo {
    /// Job identifier.
    pub id: String,
    /// Optional user label.
    pub label: String,
    /// Process id.
    pub pid: Option<u32>,
    /// Full log path.
    pub log_path: String,
    /// Current status.
    pub status: JobStatus,
    /// Start time.
    pub started: Instant,
}

/// A bounded read of one job log.
#[derive(Debug, Clone)]
pub struct JobLog {
    /// Job identifier.
    pub id: String,
    /// Current status.
    pub status: JobStatus,
    /// Byte offset used.
    pub offset: u64,
    /// Offset for the next read.
    pub next_offset: u64,
    /// Whether old bytes were discarded by rotation.
    pub truncated: bool,
    /// UTF-8 lossy log content.
    pub content: String,
    /// Full log path.
    pub log_path: String,
}

struct LogBuffer {
    bytes: Vec<u8>,
    total: u64,
    path: PathBuf,
}

impl LogBuffer {
    fn new(path: PathBuf) -> Self {
        Self {
            bytes: Vec::new(),
            total: 0,
            path,
        }
    }

    fn append(&mut self, data: &[u8]) {
        self.total = self.total.saturating_add(data.len() as u64);
        self.bytes.extend_from_slice(data);
        if self.bytes.len() > MAX_LOG_BYTES {
            let keep = MAX_LOG_BYTES / 2;
            let start = self.bytes.len() - keep;
            self.bytes.drain(..start);
        }
        let _ = std::fs::write(&self.path, &self.bytes);
    }

    fn read(&self, offset: u64, limit: usize, tail: bool) -> (u64, u64, bool, String) {
        let base = self.total.saturating_sub(self.bytes.len() as u64);
        let mut truncated = false;
        let mut start = if tail {
            self.bytes.len().saturating_sub(limit)
        } else if offset <= base {
            truncated = offset < base;
            0
        } else {
            (offset - base) as usize
        };
        if start > self.bytes.len() {
            start = self.bytes.len();
        }
        let end = (start + limit.min(MAX_LOG_READ_BYTES)).min(self.bytes.len());
        let next = base.saturating_add(end as u64);
        let content = String::from_utf8_lossy(&self.bytes[start..end]).into_owned();
        (base.saturating_add(start as u64), next, truncated, content)
    }
}

struct Job {
    pid: Option<u32>,
    child: Option<Child>,
    status: JobStatus,
    deadline: Instant,
    log: Arc<Mutex<LogBuffer>>,
    log_path: PathBuf,
}

impl Job {
    fn refresh(&mut self) {
        if self.status != JobStatus::Running {
            return;
        }
        if Instant::now() >= self.deadline {
            self.terminate(JobStatus::TimedOut);
            return;
        }
        let status = self
            .child
            .as_mut()
            .and_then(|child| child.try_wait().ok().flatten());
        if let Some(status) = status {
            self.status = JobStatus::Exited(status.code());
            self.child = None;
        }
    }

    fn terminate(&mut self, status: JobStatus) {
        if self.status != JobStatus::Running {
            return;
        }
        #[cfg(unix)]
        if let Some(pid) = self.pid {
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(pid as i32),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.child = None;
        self.status = status;
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        if self.status == JobStatus::Running {
            self.terminate(JobStatus::Stopped);
        }
    }
}

/// Owns jobs for one session tool registry.
pub struct JobManager {
    log_dir: PathBuf,
    jobs: Mutex<HashMap<String, Arc<Mutex<Job>>>>,
    next_id: AtomicU64,
    max_jobs: usize,
}

impl JobManager {
    /// Create a manager and clean stale logs from previous crashed sessions.
    pub fn new(jail: &PathJail) -> Result<Self, ToolError> {
        let log_dir = jail.resolve("target/thais-jobs")?;
        std::fs::create_dir_all(&log_dir).map_err(|source| ToolError::Io {
            path: log_dir.clone(),
            source,
        })?;
        cleanup_orphan_logs(&log_dir);
        Ok(Self {
            log_dir,
            jobs: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            max_jobs: MAX_JOBS,
        })
    }

    /// Start one background job.
    pub fn start(
        &self,
        command: &str,
        cwd: &Path,
        workspace: &Path,
        sandbox: &Sandbox,
        label: Option<String>,
        timeout: Duration,
    ) -> Result<JobInfo, ToolError> {
        if command.len() > 8_000 {
            return Err(ToolError::InvalidArgs(
                "command is too long; maximum is 8000 bytes".to_owned(),
            ));
        }
        if !metadata(cwd)?.is_dir() {
            return Err(ToolError::InvalidArgs(format!(
                "{} is not a directory",
                cwd.display()
            )));
        }
        let running = self.running_count()?;
        if running >= self.max_jobs {
            return Err(ToolError::InvalidArgs(format!(
                "job limit reached ({})",
                self.max_jobs
            )));
        }
        let id = format!("job_{}", self.next_id.fetch_add(1, Ordering::Relaxed));
        let label = label.unwrap_or_else(|| command.chars().take(80).collect());
        if label.chars().count() > 100 {
            return Err(ToolError::InvalidArgs(
                "label is too long; maximum is 100 characters".to_owned(),
            ));
        }
        let log_path = self.log_dir.join(format!("{id}.log"));
        let log = Arc::new(Mutex::new(LogBuffer::new(log_path.clone())));

        let mut process = sandbox
            .command(command, workspace, cwd)
            .map_err(ToolError::Command)?;
        let child = process
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        scrub_environment(child);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            child.process_group(0);
        }
        let mut child = child
            .spawn()
            .map_err(|error| ToolError::Command(format!("could not spawn job: {error}")))?;
        let pid = child.id();
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ToolError::Command("stdout pipe unavailable".to_owned()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| ToolError::Command("stderr pipe unavailable".to_owned()))?;
        spawn_reader(stdout, Arc::clone(&log));
        spawn_reader(stderr, Arc::clone(&log));

        let timeout = timeout.clamp(Duration::from_millis(100), MAX_JOB_TIMEOUT);
        let job = Arc::new(Mutex::new(Job {
            pid: Some(pid),
            child: Some(child),
            status: JobStatus::Running,
            deadline: Instant::now() + timeout,
            log,
            log_path: log_path.clone(),
        }));
        self.jobs
            .lock()
            .map_err(|_| ToolError::Command("job manager lock poisoned".to_owned()))?
            .insert(id.clone(), Arc::clone(&job));
        spawn_monitor(&job);
        Ok(JobInfo {
            id,
            label,
            pid: Some(pid),
            log_path: log_path.display().to_string(),
            status: JobStatus::Running,
            started: Instant::now(),
        })
    }

    /// Read a bounded slice of a job log.
    pub fn logs(
        &self,
        id: &str,
        offset: u64,
        limit: usize,
        tail: bool,
    ) -> Result<JobLog, ToolError> {
        let job = self.job(id)?;
        let mut job = job
            .lock()
            .map_err(|_| ToolError::Command("job lock poisoned".to_owned()))?;
        job.refresh();
        let (start, next, truncated, content) = job
            .log
            .lock()
            .map_err(|_| ToolError::Command("job log lock poisoned".to_owned()))?
            .read(offset, limit, tail);
        Ok(JobLog {
            id: id.to_owned(),
            status: job.status,
            offset: start,
            next_offset: next,
            truncated,
            content,
            log_path: job.log_path.display().to_string(),
        })
    }

    /// Stop a job with SIGTERM or SIGKILL.
    pub fn stop(&self, id: &str, signal: &str) -> Result<JobStatus, ToolError> {
        let job = self.job(id)?;
        let mut job = job
            .lock()
            .map_err(|_| ToolError::Command("job lock poisoned".to_owned()))?;
        if job.status == JobStatus::Running {
            #[cfg(unix)]
            if let Some(pid) = job.pid {
                let signal = if signal == "kill" {
                    nix::sys::signal::Signal::SIGKILL
                } else {
                    nix::sys::signal::Signal::SIGTERM
                };
                let _ = nix::sys::signal::killpg(nix::unistd::Pid::from_raw(pid as i32), signal);
            }
            if let Some(child) = job.child.as_mut() {
                let _ = child.kill();
                let _ = child.wait();
            }
            job.child = None;
            job.status = JobStatus::Stopped;
        }
        Ok(job.status)
    }

    /// Stop every job; used by tests and registry teardown.
    pub fn shutdown(&self) {
        if let Ok(jobs) = self.jobs.lock() {
            for job in jobs.values() {
                if let Ok(mut job) = job.lock() {
                    job.terminate(JobStatus::Stopped);
                }
            }
        }
    }

    fn job(&self, id: &str) -> Result<Arc<Mutex<Job>>, ToolError> {
        self.jobs
            .lock()
            .map_err(|_| ToolError::Command("job manager lock poisoned".to_owned()))?
            .get(id)
            .cloned()
            .ok_or_else(|| ToolError::InvalidArgs(format!("unknown job: {id}")))
    }

    fn running_count(&self) -> Result<usize, ToolError> {
        let jobs = self
            .jobs
            .lock()
            .map_err(|_| ToolError::Command("job manager lock poisoned".to_owned()))?;
        Ok(jobs
            .values()
            .filter(|job| {
                job.lock()
                    .map(|job| job.status == JobStatus::Running)
                    .unwrap_or(false)
            })
            .count())
    }
}

impl Drop for JobManager {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn spawn_reader(mut reader: impl Read + Send + 'static, log: Arc<Mutex<LogBuffer>>) {
    std::thread::spawn(move || {
        let mut buffer = [0_u8; 8192];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    if let Ok(mut log) = log.lock() {
                        log.append(&buffer[..read]);
                    }
                }
            }
        }
    });
}

fn spawn_monitor(job: &Arc<Mutex<Job>>) {
    let weak = Arc::downgrade(job);
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(Duration::from_millis(50));
            let Some(job) = weak.upgrade() else {
                break;
            };
            let Ok(mut job) = job.lock() else {
                break;
            };
            job.refresh();
            if job.status != JobStatus::Running {
                break;
            }
        }
    });
}

fn cleanup_orphan_logs(log_dir: &Path) {
    let Ok(entries) = std::fs::read_dir(log_dir) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("log") {
            continue;
        }
        let old = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age > ORPHAN_LOG_AGE);
        if old {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Start a background shell command.
pub struct BashStartTool {
    manager: Arc<JobManager>,
    sandbox: Sandbox,
}

impl BashStartTool {
    /// Build a start tool.
    #[must_use]
    pub fn new(manager: Arc<JobManager>, sandbox: Sandbox) -> Self {
        Self { manager, sandbox }
    }
}

impl Tool for BashStartTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "bash_start".to_owned(),
            description:
                "Start a shell command in the background. The process uses the same sandbox and environment scrub as bash. Use bash_logs and bash_stop."
                    .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "minLength": 1},
                    "cwd": {"type": "string"},
                    "label": {"type": "string", "maxLength": 100}
                },
                "required": ["command"],
                "additionalProperties": false
            }),
        }
    }

    fn approval(&self) -> Approval {
        Approval::Ask
    }

    fn summary(&self, args: &Value) -> String {
        format!(
            "bash_start: {}",
            args.get("label")
                .and_then(Value::as_str)
                .or_else(|| args.get("command").and_then(Value::as_str))
                .unwrap_or("command")
                .chars()
                .take(200)
                .collect::<String>()
        )
    }

    fn execute(&self, args: Value, jail: &PathJail) -> Result<ToolResult, ToolError> {
        let command = required_str(&args, "command")?;
        let cwd = jail.resolve(args.get("cwd").and_then(Value::as_str).unwrap_or("."))?;
        let label = args.get("label").and_then(Value::as_str).map(str::to_owned);
        let info = self.manager.start(
            command,
            &cwd,
            jail.root(),
            &self.sandbox,
            label,
            DEFAULT_JOB_TIMEOUT,
        )?;
        Ok(ToolResult::ok(cap_output(format!(
            "job_id: {}\nstatus: {}\npid: {}\nlog: {}\n",
            info.id,
            info.status.as_str(),
            info.pid.unwrap_or_default(),
            info.log_path
        ))))
    }
}

/// Read logs from a background job.
pub struct BashLogsTool {
    manager: Arc<JobManager>,
}

impl BashLogsTool {
    /// Build a logs tool.
    #[must_use]
    pub fn new(manager: Arc<JobManager>) -> Self {
        Self { manager }
    }
}

impl Tool for BashLogsTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "bash_logs".to_owned(),
            description:
                "Read a bounded slice of a background job log. Use offset to follow, or tail=true for the newest bytes."
                    .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "job_id": {"type": "string", "minLength": 1},
                    "offset": {"type": "integer", "minimum": 0},
                    "tail": {"type": "boolean"},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 64000}
                },
                "required": ["job_id"],
                "additionalProperties": false
            }),
        }
    }

    fn execute(&self, args: Value, jail: &PathJail) -> Result<ToolResult, ToolError> {
        let id = required_str(&args, "job_id")?;
        let offset = args.get("offset").and_then(Value::as_u64).unwrap_or(0);
        let tail = args.get("tail").and_then(Value::as_bool).unwrap_or(false);
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .map_or(DEFAULT_LOG_READ_BYTES, |limit| {
                limit.clamp(1, MAX_LOG_READ_BYTES as u64) as usize
            });
        let log = self.manager.logs(id, offset, limit, tail)?;
        let content = if log.content.is_empty() {
            "(no new log output)\n".to_owned()
        } else {
            log.content
        };
        Ok(ToolResult::ok(cap_output(format!(
            "job_id: {}\nstatus: {}\noffset: {}\nnext_offset: {}\ntruncated: {}\nlog: {}\ncontent:\n{}",
            log.id,
            log.status.as_str(),
            log.offset,
            log.next_offset,
            log.truncated,
            jail.display(Path::new(&log.log_path)),
            content
        ))))
    }
}

/// Stop a background job.
pub struct BashStopTool {
    manager: Arc<JobManager>,
}

impl BashStopTool {
    /// Build a stop tool.
    #[must_use]
    pub fn new(manager: Arc<JobManager>) -> Self {
        Self { manager }
    }
}

impl Tool for BashStopTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "bash_stop".to_owned(),
            description: "Stop a background job started by this session.".to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "job_id": {"type": "string", "minLength": 1},
                    "signal": {"type": "string", "enum": ["term", "kill"]}
                },
                "required": ["job_id"],
                "additionalProperties": false
            }),
        }
    }

    fn summary(&self, args: &Value) -> String {
        format!(
            "bash_stop {}",
            args.get("job_id").and_then(Value::as_str).unwrap_or("")
        )
    }

    fn execute(&self, args: Value, _jail: &PathJail) -> Result<ToolResult, ToolError> {
        let id = required_str(&args, "job_id")?;
        let signal = args.get("signal").and_then(Value::as_str).unwrap_or("term");
        if !matches!(signal, "term" | "kill") {
            return Err(ToolError::InvalidArgs(
                "signal must be term or kill".to_owned(),
            ));
        }
        let status = self.manager.stop(id, signal)?;
        Ok(ToolResult::ok(format!(
            "job_id: {}\nstatus: {}\n",
            id,
            status.as_str()
        )))
    }
}

/// Build the start/logs/stop tool set.
pub(crate) fn tools(
    manager: Arc<JobManager>,
    sandbox: Sandbox,
) -> Vec<(&'static str, Arc<dyn Tool>)> {
    vec![
        (
            "bash_start",
            Arc::new(BashStartTool::new(Arc::clone(&manager), sandbox)),
        ),
        (
            "bash_logs",
            Arc::new(BashLogsTool::new(Arc::clone(&manager))),
        ),
        ("bash_stop", Arc::new(BashStopTool::new(manager))),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::{Sandbox, SandboxMode};

    fn manager(temp: &tempfile::TempDir) -> JobManager {
        let jail = PathJail::new(temp.path()).expect("jail");
        JobManager::new(&jail).expect("manager")
    }

    fn sandbox() -> Sandbox {
        Sandbox::detect(SandboxMode::Off)
    }

    fn wait_status(manager: &JobManager, id: &str) -> JobLog {
        for _ in 0..100 {
            let log = manager.logs(id, 0, MAX_LOG_READ_BYTES, true).expect("logs");
            if log.status != JobStatus::Running {
                return log;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("job did not finish");
    }

    #[test]
    fn starts_reads_and_stops_a_job() {
        let temp = tempfile::tempdir().expect("temp");
        let manager = manager(&temp);
        let job = manager
            .start(
                "printf hello; sleep 0.1; printf world",
                temp.path(),
                temp.path(),
                &sandbox(),
                Some("echo".to_owned()),
                Duration::from_secs(5),
            )
            .expect("start");
        let log = wait_status(&manager, &job.id);
        assert_eq!(log.status, JobStatus::Exited(Some(0)));
        assert!(log.content.contains("hello"));
        assert!(log.content.contains("world"));
        assert!(std::path::Path::new(&log.log_path).is_file());
    }

    #[test]
    fn stop_kills_the_process_group() {
        let temp = tempfile::tempdir().expect("temp");
        let manager = manager(&temp);
        let job = manager
            .start(
                "sleep 30",
                temp.path(),
                temp.path(),
                &sandbox(),
                None,
                Duration::from_secs(30),
            )
            .expect("start");
        assert_eq!(
            manager.stop(&job.id, "kill").expect("stop"),
            JobStatus::Stopped
        );
        let log = wait_status(&manager, &job.id);
        assert_eq!(log.status, JobStatus::Stopped);
        #[cfg(unix)]
        assert!(
            nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(job.pid.expect("pid") as i32),
                None
            )
            .is_err()
        );
    }

    #[test]
    fn timeout_kills_the_process_group() {
        let temp = tempfile::tempdir().expect("temp");
        let manager = manager(&temp);
        let job = manager
            .start(
                "sleep 30",
                temp.path(),
                temp.path(),
                &sandbox(),
                None,
                Duration::from_millis(100),
            )
            .expect("start");
        let log = wait_status(&manager, &job.id);
        assert_eq!(log.status, JobStatus::TimedOut);
    }

    #[cfg(unix)]
    #[test]
    fn drop_kills_running_jobs() {
        let temp = tempfile::tempdir().expect("temp");
        let manager = manager(&temp);
        let job = manager
            .start(
                "sleep 30",
                temp.path(),
                temp.path(),
                &sandbox(),
                None,
                Duration::from_secs(30),
            )
            .expect("start");
        let pid = job.pid.expect("pid");
        drop(manager);
        std::thread::sleep(Duration::from_millis(100));
        assert!(nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None).is_err());
    }

    #[test]
    fn logs_are_bounded_and_rotated() {
        let temp = tempfile::tempdir().expect("temp");
        let manager = manager(&temp);
        let job = manager
            .start(
                "yes x | head -c 2500000",
                temp.path(),
                temp.path(),
                &sandbox(),
                None,
                Duration::from_secs(10),
            )
            .expect("start");
        let log = wait_status(&manager, &job.id);
        assert_eq!(log.status, JobStatus::Exited(Some(0)));
        assert!(log.content.len() <= MAX_LOG_READ_BYTES);
        assert!(
            std::path::Path::new(&log.log_path)
                .metadata()
                .expect("log")
                .len() as usize
                <= MAX_LOG_BYTES
        );
    }

    #[test]
    fn jobs_do_not_inherit_the_parent_environment() {
        let temp = tempfile::tempdir().expect("temp");
        let manager = manager(&temp);
        let job = manager
            .start(
                "env",
                temp.path(),
                temp.path(),
                &sandbox(),
                None,
                Duration::from_secs(5),
            )
            .expect("start");
        let log = wait_status(&manager, &job.id);
        assert!(!log.content.contains("CARGO_HOME="));
        assert!(!log.content.contains("RUSTUP_HOME="));
        assert!(log.content.contains("PATH="));
    }

    #[test]
    fn bash_start_logs_and_stop_round_trip_through_the_registry() {
        let temp = tempfile::tempdir().expect("temp");
        let registry = crate::tools::ToolRegistry::read_write(temp.path()).expect("registry");
        let started = registry
            .execute(
                "bash_start",
                json!({"command": "printf one; sleep 0.05; printf two", "label": "echo"}),
            )
            .expect("start");
        let id = started
            .content
            .lines()
            .find_map(|line| line.strip_prefix("job_id: "))
            .expect("job id")
            .to_owned();
        let mut content = String::new();
        for _ in 0..50 {
            let logs = registry
                .execute("bash_logs", json!({"job_id": id, "tail": true}))
                .expect("logs");
            content = logs.content;
            if content.contains("status: exited") && content.contains("two") {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(content.contains("one"), "{content}");
        assert!(content.contains("two"), "{content}");
        assert!(content.contains("status: exited"), "{content}");

        let started = registry
            .execute("bash_start", json!({"command": "sleep 30"}))
            .expect("start");
        let id = started
            .content
            .lines()
            .find_map(|line| line.strip_prefix("job_id: "))
            .expect("job id");
        let stopped = registry
            .execute("bash_stop", json!({"job_id": id, "signal": "kill"}))
            .expect("stop");
        assert!(
            stopped.content.contains("status: stopped"),
            "{}",
            stopped.content
        );
    }

    #[test]
    fn start_tool_is_registered_and_asks_without_mutating() {
        let temp = tempfile::tempdir().expect("temp");
        let registry = crate::tools::ToolRegistry::read_write(temp.path()).expect("registry");
        assert!(registry.is_known("bash_start"));
        assert!(registry.is_known("bash_logs"));
        assert!(registry.is_known("bash_stop"));
        assert!(!registry.is_mutating("bash_start"));
        assert_eq!(registry.approval("bash_start"), Approval::Ask);
    }
}
