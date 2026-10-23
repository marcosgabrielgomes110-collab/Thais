use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::Value;
use thais_protocol::event::{EventEnvelope, EventKind};

fn binary() -> std::path::PathBuf {
    assert_cmd::cargo::cargo_bin("thais")
}

fn with_xdg(command: &mut Command, root: &Path) {
    command
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("XDG_RUNTIME_DIR", root.join("runtime"))
        .env("DBUS_SESSION_BUS_ADDRESS", "unix:path=/nonexistent");
}

#[test]
fn run_emits_json_events_and_human_output() {
    let temp = tempfile::tempdir().expect("temp");
    let mut command = Command::new(binary());
    with_xdg(&mut command, temp.path());
    let output = command
        .args(["run", "--task", "hello", "--json"])
        .output()
        .expect("run");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let events: Vec<EventEnvelope> = String::from_utf8(output.stdout)
        .expect("stdout")
        .lines()
        .map(|line| serde_json::from_str(line).expect("event"))
        .collect();

    let kinds: Vec<EventKind> = events.iter().map(|event| event.kind).collect();
    assert_eq!(
        kinds,
        vec![
            EventKind::SessionCreated,
            EventKind::TurnStarted,
            EventKind::ModelDelta,
            EventKind::TurnCompleted,
        ]
    );
    assert_eq!(events[2].payload["text"], "M0 fake provider ready.");

    let mut command = Command::new(binary());
    with_xdg(&mut command, temp.path());
    let output = command
        .args(["run", "--task", "hello"])
        .output()
        .expect("human run");
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).expect("stdout").trim(),
        "M0 fake provider ready."
    );
}

#[test]
fn doctor_reports_paths_and_fts5() {
    let temp = tempfile::tempdir().expect("temp");
    let mut command = Command::new(binary());
    with_xdg(&mut command, temp.path());
    let output = command.arg("doctor").output().expect("doctor");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout");
    assert!(stdout.contains("SQLite FTS5 available"), "{stdout}");
    assert!(stdout.contains("schema version: 1"), "{stdout}");
    assert!(
        stdout.contains(temp.path().to_string_lossy().as_ref()),
        "{stdout}"
    );
}

#[test]
fn serve_stdio_completes_the_handshake() {
    let temp = tempfile::tempdir().expect("temp");
    let mut command = Command::new(binary());
    with_xdg(&mut command, temp.path());
    let mut child = command
        .args(["serve", "--stdio"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut stdout = BufReader::new(stdout);

    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "client_name": "cli-test",
            "client_version": "0.1",
            "protocol_version": "1.0",
            "capabilities": {}
        }
    });
    stdin
        .write_all(format!("{request}\n").as_bytes())
        .expect("write");
    stdin.flush().expect("flush");

    let mut line = String::new();
    stdout.read_line(&mut line).expect("read");
    let response: Value = serde_json::from_str(&line).expect("json");
    assert_eq!(response["id"], 1);
    assert_eq!(response["result"]["protocol_version"], "1.0");
    assert_eq!(response["result"]["server_name"], "thais");

    drop(stdin);
    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[cfg(target_os = "linux")]
#[test]
fn auth_falls_back_without_leaking_the_secret() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().expect("temp");
    let mut command = Command::new(binary());
    with_xdg(&mut command, temp.path());
    let mut child = command
        .args(["auth", "login", "openai", "--account", "default"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(b"sk-test-secret\n")
        .expect("write");
    let output = child.wait_with_output().expect("wait");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("stdout");
    assert!(stdout.contains("stored openai:default"), "{stdout}");
    assert!(!stdout.contains("sk-test-secret"));

    let credentials = temp.path().join("config/thais/credentials.toml");
    assert!(credentials.exists(), "expected the 0600 fallback file");
    let mode = std::fs::metadata(&credentials)
        .expect("metadata")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);

    let mut command = Command::new(binary());
    with_xdg(&mut command, temp.path());
    let output = command.args(["auth", "status"]).output().expect("status");
    assert!(output.status.success());
    let status = String::from_utf8(output.stdout).expect("stdout");
    assert!(!status.contains("sk-test-secret"));
    assert!(status.contains("fallback file: present"), "{status}");

    let mut command = Command::new(binary());
    with_xdg(&mut command, temp.path());
    let output = command
        .args(["auth", "logout", "openai", "--account", "default"])
        .output()
        .expect("logout");
    assert!(output.status.success());
    assert!(!credentials.exists());
}

#[test]
fn trust_round_trip_invalidates_on_config_change() {
    let temp = tempfile::tempdir().expect("temp");
    let workspace = temp.path().join("repo");
    std::fs::create_dir_all(workspace.join(".thais")).expect("config dir");
    let config = workspace.join(".thais/config.toml");
    std::fs::write(&config, "theme = \"dark\"\n").expect("config");

    let mut command = Command::new(binary());
    with_xdg(&mut command, temp.path());
    let output = command
        .arg("trust")
        .current_dir(&workspace)
        .output()
        .expect("trust");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let mut command = Command::new(binary());
    with_xdg(&mut command, temp.path());
    let output = command
        .args(["trust", "--status"])
        .current_dir(&workspace)
        .output()
        .expect("trust status");
    assert!(output.status.success());
    assert!(
        String::from_utf8(output.stdout)
            .expect("stdout")
            .contains("trusted: true")
    );

    std::fs::write(&config, "theme = \"light\"\n").expect("config");
    let mut command = Command::new(binary());
    with_xdg(&mut command, temp.path());
    let output = command
        .args(["trust", "--status"])
        .current_dir(&workspace)
        .output()
        .expect("trust status");
    assert!(output.status.success());
    assert!(
        String::from_utf8(output.stdout)
            .expect("stdout")
            .contains("trusted: false")
    );
}
