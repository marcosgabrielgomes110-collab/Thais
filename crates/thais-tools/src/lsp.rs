//! Minimal LSP client used for diagnostics, definitions, and references.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{Value, json};

use crate::error::ToolError;
use crate::path_jail::PathJail;
use crate::tools::{Tool, ToolDefinition, ToolResult, cap_output};

/// LSP configuration loaded from user config.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LspConfig {
    /// Configured servers.
    #[serde(default)]
    pub servers: Vec<LspServerConfig>,
}

/// One LSP server.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LspServerConfig {
    /// Stable server id.
    pub id: String,
    /// Executable.
    pub command: String,
    /// Arguments.
    #[serde(default)]
    pub args: Vec<String>,
    /// File extensions handled by this server, without the dot.
    #[serde(default)]
    pub extensions: Vec<String>,
    /// Root markers. At least one must exist in the workspace.
    #[serde(default)]
    pub root_markers: Vec<String>,
    /// Request timeout in milliseconds.
    #[serde(default = "default_timeout")]
    pub timeout_ms: u64,
}

impl LspConfig {
    /// Load `lsp.toml`; a missing file means no LSP.
    pub fn load(path: &Path) -> Result<Self, LspError> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(source) => {
                return Err(LspError::Io {
                    path: path.to_path_buf(),
                    source,
                });
            }
        };
        Ok(toml::from_str(&text)?)
    }

    /// Whether any server is configured.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.servers.is_empty()
    }

    fn for_path(&self, root: &Path, path: &Path) -> Option<LspServerConfig> {
        let extension = path.extension().and_then(|extension| extension.to_str())?;
        self.servers
            .iter()
            .find(|server| {
                server
                    .extensions
                    .iter()
                    .any(|candidate| candidate.eq_ignore_ascii_case(extension))
                    && (server.root_markers.is_empty()
                        || server
                            .root_markers
                            .iter()
                            .any(|marker| root.join(marker).exists()))
            })
            .cloned()
    }
}

/// Diagnostics returned by a language server.
#[derive(Debug, Clone)]
pub struct LspReport {
    /// Human-readable diagnostic text.
    pub text: String,
    /// Whether any diagnostic has error severity.
    pub has_errors: bool,
}

/// One managed LSP server.
pub struct LspManager {
    root: PathBuf,
    config: LspConfig,
    servers: HashMap<String, LspServer>,
}

impl LspManager {
    /// Build a lazy manager for a workspace.
    #[must_use]
    pub fn new(root: PathBuf, config: LspConfig) -> Self {
        Self {
            root,
            config,
            servers: HashMap::new(),
        }
    }

    /// Diagnose one local file.
    pub fn diagnostics(&mut self, path: &Path) -> Result<LspReport, LspError> {
        let server_config = self
            .config
            .for_path(&self.root, path)
            .ok_or_else(|| LspError::NoServer(path.to_path_buf()))?;
        let value = self.with_server(&server_config, |server| server.diagnostics(path))?;
        let diagnostics = value.as_array().cloned().unwrap_or_default();
        let has_errors = diagnostics.iter().any(|diagnostic| {
            diagnostic
                .get("severity")
                .and_then(Value::as_i64)
                .is_some_and(|severity| severity == 1)
        });
        let mut text = String::new();
        for diagnostic in diagnostics {
            let severity = match diagnostic.get("severity").and_then(Value::as_i64) {
                Some(1) => "error",
                Some(2) => "warning",
                Some(3) => "info",
                _ => "hint",
            };
            let line = diagnostic
                .pointer("/range/start/line")
                .and_then(Value::as_u64)
                .map_or(0, |line| line + 1);
            let message = diagnostic
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("");
            text.push_str(&format!("{severity}:{line}: {message}\n"));
        }
        if text.is_empty() {
            text.push_str("(no diagnostics)\n");
        }
        Ok(LspReport {
            text: cap_output(text),
            has_errors,
        })
    }

    /// Resolve definitions for a file position.
    pub fn definition(
        &mut self,
        path: &Path,
        line: u64,
        character: u64,
    ) -> Result<Value, LspError> {
        let server_config = self
            .config
            .for_path(&self.root, path)
            .ok_or_else(|| LspError::NoServer(path.to_path_buf()))?;
        self.with_server(&server_config, |server| {
            server.open(path)?;
            server.request(
                "textDocument/definition",
                json!({
                    "textDocument": {"uri": file_uri(path)},
                    "position": {"line": line.saturating_sub(1), "character": character}
                }),
            )
        })
    }

    /// Resolve references for a file position.
    pub fn references(
        &mut self,
        path: &Path,
        line: u64,
        character: u64,
    ) -> Result<Value, LspError> {
        let server_config = self
            .config
            .for_path(&self.root, path)
            .ok_or_else(|| LspError::NoServer(path.to_path_buf()))?;
        self.with_server(&server_config, |server| {
            server.open(path)?;
            server.request(
                "textDocument/references",
                json!({
                    "textDocument": {"uri": file_uri(path)},
                    "position": {"line": line.saturating_sub(1), "character": character},
                    "context": {"includeDeclaration": true}
                }),
            )
        })
    }

    fn with_server<T>(
        &mut self,
        config: &LspServerConfig,
        operation: impl FnOnce(&mut LspServer) -> Result<T, LspError>,
    ) -> Result<T, LspError> {
        if !self.servers.contains_key(&config.id) {
            self.servers
                .insert(config.id.clone(), LspServer::start(&self.root, config)?);
        }
        let result = {
            let server = self
                .servers
                .get_mut(&config.id)
                .ok_or_else(|| LspError::Message("server disappeared".to_owned()))?;
            operation(server)
        };
        if result.is_err() {
            self.servers.remove(&config.id);
        }
        result
    }
}

struct LspServer {
    child: Child,
    stdin: ChildStdin,
    receiver: Receiver<Value>,
    diagnostics: HashMap<String, Vec<Value>>,
    next_id: i64,
    timeout: Duration,
}

impl Drop for LspServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl LspServer {
    fn start(root: &Path, config: &LspServerConfig) -> Result<Self, LspError> {
        let mut child = Command::new(&config.command)
            .args(&config.args)
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|source| LspError::Io {
                path: PathBuf::from(&config.command),
                source,
            })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| LspError::Message("LSP stdin missing".to_owned()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| LspError::Message("LSP stdout missing".to_owned()))?;
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            while let Some(value) = read_lsp_message(&mut reader) {
                if sender.send(value).is_err() {
                    break;
                }
            }
        });
        let mut server = Self {
            child,
            stdin,
            receiver,
            diagnostics: HashMap::new(),
            next_id: 1,
            timeout: Duration::from_millis(config.timeout_ms.clamp(100, 30_000)),
        };
        let root_uri = file_uri(root);
        let response = server.request(
            "initialize",
            json!({
                "processId": std::process::id(),
                "rootUri": root_uri,
                "capabilities": {
                    "textDocument": {
                        "publishDiagnostics": {},
                        "definition": {},
                        "references": {}
                    }
                },
                "clientInfo": {"name": "thais", "version": env!("CARGO_PKG_VERSION")}
            }),
        )?;
        let _ = response;
        server.notify("initialized", json!({}))?;
        Ok(server)
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value, LspError> {
        let id = self.next_id;
        self.next_id += 1;
        self.write_message(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        }))?;
        let deadline = Instant::now() + self.timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(LspError::Timeout(method.to_owned()));
            }
            match self.receiver.recv_timeout(remaining) {
                Ok(value) => {
                    if value.get("id").and_then(Value::as_i64) == Some(id) {
                        if let Some(error) = value.get("error") {
                            return Err(LspError::Message(error.to_string()));
                        }
                        return Ok(value.get("result").cloned().unwrap_or(Value::Null));
                    }
                    self.handle_message(value);
                }
                Err(RecvTimeoutError::Timeout) => {
                    return Err(LspError::Timeout(method.to_owned()));
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(LspError::Message("LSP server exited".to_owned()));
                }
            }
        }
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<(), LspError> {
        self.write_message(&json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params
        }))
    }

    fn write_message(&mut self, value: &Value) -> Result<(), LspError> {
        let body = serde_json::to_vec(value)?;
        write!(self.stdin, "Content-Length: {}\r\n\r\n", body.len())
            .map_err(|error| LspError::Message(format!("LSP write: {error}")))?;
        self.stdin
            .write_all(&body)
            .and_then(|()| self.stdin.flush())
            .map_err(|error| LspError::Message(format!("LSP write: {error}")))
    }

    fn handle_message(&mut self, value: Value) {
        if value.get("method").and_then(Value::as_str) == Some("textDocument/publishDiagnostics") {
            let uri = value
                .pointer("/params/uri")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let diagnostics = value
                .pointer("/params/diagnostics")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            self.diagnostics.insert(uri, diagnostics);
        }
    }

    fn open(&mut self, path: &Path) -> Result<(), LspError> {
        let text = std::fs::read_to_string(path).map_err(|source| LspError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let language_id = path
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or("text");
        self.notify(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": file_uri(path),
                    "languageId": language_id,
                    "version": 1,
                    "text": text
                }
            }),
        )
    }

    fn diagnostics(&mut self, path: &Path) -> Result<Value, LspError> {
        self.open(path)?;
        let uri = file_uri(path);
        let deadline = Instant::now() + self.timeout;
        loop {
            if let Some(diagnostics) = self.diagnostics.get(&uri) {
                return Ok(Value::Array(diagnostics.clone()));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(LspError::Timeout(
                    "textDocument/publishDiagnostics".to_owned(),
                ));
            }
            match self.receiver.recv_timeout(remaining) {
                Ok(value) => self.handle_message(value),
                Err(RecvTimeoutError::Timeout) => {
                    return Err(LspError::Timeout(
                        "textDocument/publishDiagnostics".to_owned(),
                    ));
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(LspError::Message("LSP server exited".to_owned()));
                }
            }
        }
    }
}

fn read_lsp_message(reader: &mut BufReader<ChildStdout>) -> Option<Value> {
    let mut length = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let line = line.trim();
        if line.is_empty() {
            break;
        }
        if let Some(value) = line.strip_prefix("Content-Length:") {
            length = value.trim().parse::<usize>().ok();
        }
    }
    let length = length?;
    let mut body = vec![0_u8; length];
    reader.read_exact(&mut body).ok()?;
    serde_json::from_slice(&body).ok()
}

/// Diagnostics tool.
pub struct LspDiagnosticsTool {
    manager: Arc<Mutex<LspManager>>,
}

/// Definition tool.
pub struct LspDefinitionTool {
    manager: Arc<Mutex<LspManager>>,
}

/// References tool.
pub struct LspReferencesTool {
    manager: Arc<Mutex<LspManager>>,
}

macro_rules! lsp_tool {
    ($tool:ty, $name:literal, $description:literal) => {
        impl Tool for $tool {
            fn definition(&self) -> ToolDefinition {
                ToolDefinition {
                    name: $name.to_owned(),
                    description: $description.to_owned(),
                    parameters: json!({
                        "type": "object",
                        "properties": {
                            "path": {"type": "string"},
                            "line": {"type": "integer", "minimum": 1},
                            "character": {"type": "integer", "minimum": 0}
                        },
                        "required": ["path"],
                        "additionalProperties": false
                    }),
                }
            }

            fn execute(&self, args: Value, jail: &PathJail) -> Result<ToolResult, ToolError> {
                let path = args
                    .get("path")
                    .and_then(Value::as_str)
                    .map(|path| jail.resolve(path))
                    .transpose()?
                    .ok_or_else(|| ToolError::InvalidArgs("missing path".to_owned()))?;
                let line = args.get("line").and_then(Value::as_u64).unwrap_or(1);
                let character = args.get("character").and_then(Value::as_u64).unwrap_or(0);
                let mut manager = self
                    .manager
                    .lock()
                    .map_err(|_| ToolError::Command("LSP manager lock poisoned".to_owned()))?;
                let value = if stringify!($name) == "lsp_diagnostics" {
                    let report = manager
                        .diagnostics(&path)
                        .map_err(|error| ToolError::Command(error.to_string()))?;
                    return Ok(ToolResult {
                        content: report.text,
                        is_error: report.has_errors,
                        diff: None,
                    });
                } else if stringify!($name) == "lsp_definition" {
                    manager
                        .definition(&path, line, character)
                        .map_err(|error| ToolError::Command(error.to_string()))?
                } else {
                    manager
                        .references(&path, line, character)
                        .map_err(|error| ToolError::Command(error.to_string()))?
                };
                Ok(ToolResult::ok(cap_output(format_locations(&value))))
            }
        }
    };
}

lsp_tool!(
    LspDiagnosticsTool,
    "lsp_diagnostics",
    "Get language-server diagnostics for one workspace file."
);
lsp_tool!(
    LspDefinitionTool,
    "lsp_definition",
    "Find the definition at a 1-based line and 0-based character."
);
lsp_tool!(
    LspReferencesTool,
    "lsp_references",
    "Find references at a 1-based line and 0-based character."
);

fn format_locations(value: &Value) -> String {
    let locations = match value {
        Value::Array(values) => values.clone(),
        value if value.is_null() => Vec::new(),
        value => vec![value.clone()],
    };
    if locations.is_empty() {
        return "(no locations)\n".to_owned();
    }
    locations
        .iter()
        .map(|location| {
            location
                .get("uri")
                .and_then(Value::as_str)
                .unwrap_or("?")
                .to_owned()
                + ":"
                + &location
                    .pointer("/range/start/line")
                    .and_then(Value::as_u64)
                    .map_or(0, |line| line + 1)
                    .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Build the three LSP tools for a manager.
pub(crate) fn tools(manager: Arc<Mutex<LspManager>>) -> Vec<(&'static str, Arc<dyn Tool>)> {
    vec![
        (
            "lsp_diagnostics",
            Arc::new(LspDiagnosticsTool {
                manager: Arc::clone(&manager),
            }),
        ),
        (
            "lsp_definition",
            Arc::new(LspDefinitionTool {
                manager: Arc::clone(&manager),
            }),
        ),
        ("lsp_references", Arc::new(LspReferencesTool { manager })),
    ]
}

/// LSP failures.
#[derive(Debug, thiserror::Error)]
pub enum LspError {
    /// I/O failure.
    #[error("LSP I/O error at {path}: {source}")]
    Io {
        /// Path involved.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },
    /// Invalid config.
    #[error("invalid LSP config: {0}")]
    Toml(#[from] toml::de::Error),
    /// JSON failure.
    #[error("LSP JSON error: {0}")]
    Json(#[from] serde_json::Error),
    /// No server handles this file.
    #[error("no LSP server for {0}")]
    NoServer(PathBuf),
    /// Request timeout.
    #[error("LSP timeout: {0}")]
    Timeout(String),
    /// Protocol/server error.
    #[error("LSP error: {0}")]
    Message(String),
}

fn file_uri(path: &Path) -> String {
    let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let mut text = path.to_string_lossy().replace('\\', "/");
    if !text.starts_with('/') {
        text.insert(0, '/');
    }
    format!("file://{}", text.replace(' ', "%20"))
}

fn default_timeout() -> u64 {
    2_000
}

#[cfg(test)]
mod tests {
    use super::*;

    fn python_available() -> bool {
        std::process::Command::new("python3")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok()
    }

    #[test]
    fn rejects_unconfigured_extensions() {
        let config = LspConfig {
            servers: vec![LspServerConfig {
                id: "rust".to_owned(),
                command: "rust-analyzer".to_owned(),
                args: Vec::new(),
                extensions: vec!["rs".to_owned()],
                root_markers: Vec::new(),
                timeout_ms: 100,
            }],
        };
        assert!(
            config
                .for_path(Path::new("/tmp"), Path::new("/tmp/a.py"))
                .is_none()
        );
        assert!(
            config
                .for_path(Path::new("/tmp"), Path::new("/tmp/a.rs"))
                .is_some()
        );
    }

    #[test]
    fn diagnostics_definition_and_references_use_the_server() {
        if !python_available() {
            return;
        }
        let temp = tempfile::tempdir().expect("temp");
        let script = temp.path().join("fake_lsp.py");
        std::fs::write(
            &script,
            r#"
import json, sys
def read():
    headers = {}
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            return None
        if line in (b"\r\n", b"\n"):
            break
        key, value = line.decode().split(":", 1)
        headers[key.lower()] = value.strip()
    body = sys.stdin.buffer.read(int(headers["content-length"]))
    return json.loads(body)
def write(value):
    body = json.dumps(value).encode()
    sys.stdout.buffer.write(("Content-Length: %d\r\n\r\n" % len(body)).encode() + body)
    sys.stdout.buffer.flush()
while True:
    message = read()
    if message is None:
        break
    method = message.get("method")
    ident = message.get("id")
    if method == "initialize":
        write({"jsonrpc":"2.0","id":ident,"result":{"capabilities":{}}})
    elif method == "textDocument/didOpen":
        uri = message["params"]["textDocument"]["uri"]
        write({"jsonrpc":"2.0","method":"textDocument/publishDiagnostics","params":{"uri":uri,"diagnostics":[{"range":{"start":{"line":0,"character":0}},"severity":1,"message":"fake error"}]}})
    elif method == "textDocument/definition":
        write({"jsonrpc":"2.0","id":ident,"result":[{"uri":"file:///tmp/def.rs","range":{"start":{"line":1,"character":0}}}]})
    elif method == "textDocument/references":
        write({"jsonrpc":"2.0","id":ident,"result":[{"uri":"file:///tmp/ref.rs","range":{"start":{"line":2,"character":0}}}]})
"#,
        )
        .expect("script");
        let file = temp.path().join("main.txt");
        std::fs::write(&file, "hello\n").expect("file");
        let config = LspConfig {
            servers: vec![LspServerConfig {
                id: "fake".to_owned(),
                command: "python3".to_owned(),
                args: vec![script.to_string_lossy().into_owned()],
                extensions: vec!["txt".to_owned()],
                root_markers: Vec::new(),
                timeout_ms: 2_000,
            }],
        };
        let mut manager = LspManager::new(temp.path().to_path_buf(), config);
        let report = manager.diagnostics(&file).expect("diagnostics");
        assert!(report.has_errors);
        assert!(report.text.contains("fake error"));
        let definition = manager.definition(&file, 1, 0).expect("definition");
        assert!(definition.to_string().contains("def.rs"));
        let references = manager.references(&file, 1, 0).expect("references");
        assert!(references.to_string().contains("ref.rs"));
    }
}
