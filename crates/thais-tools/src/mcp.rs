//! Minimal stdio MCP client with tool discovery and namespacing.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{Value, json};

use crate::tools::{ToolDefinition, ToolResult};

/// MCP configuration loaded from user config.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpConfig {
    /// Configured servers.
    #[serde(default)]
    pub servers: Vec<McpServerConfig>,
}

/// One MCP server.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpServerConfig {
    /// Stable server id.
    pub id: String,
    /// Executable for stdio transport.
    pub command: String,
    /// Endpoint for HTTP transport.
    #[serde(default)]
    pub url: Option<String>,
    /// Arguments.
    #[serde(default)]
    pub args: Vec<String>,
    /// Request timeout.
    #[serde(default = "default_timeout")]
    pub timeout_ms: u64,
    /// Whether the server's tools are read-only and can skip approval.
    #[serde(default)]
    pub read_only: bool,
    /// Transport; only `stdio` is currently supported.
    #[serde(default = "default_transport")]
    pub transport: String,
}

impl McpConfig {
    /// Load `mcp.toml`; missing means no MCP.
    pub fn load(path: &Path) -> Result<Self, McpError> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(source) => {
                return Err(McpError::Io {
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
}

/// One discovered MCP tool.
#[derive(Debug, Clone)]
pub struct McpTool {
    /// Namespaced tool name.
    pub name: String,
    /// Description.
    pub description: String,
    /// Input schema.
    pub input_schema: Value,
    /// Server id.
    pub server: String,
    /// Original tool name.
    pub tool: String,
    /// Whether the server is configured read-only.
    pub read_only: bool,
}

impl McpTool {
    /// Convert to the internal tool definition.
    #[must_use]
    pub fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name.clone(),
            description: format!("[MCP {}] {}", self.server, self.description),
            parameters: self.input_schema.clone(),
        }
    }
}

/// Lazy stdio MCP manager.
pub struct McpManager {
    config: McpConfig,
    servers: HashMap<String, McpServer>,
    tools: Vec<McpTool>,
}

impl McpManager {
    /// Build a manager.
    #[must_use]
    pub fn new(config: McpConfig) -> Self {
        Self {
            config,
            servers: HashMap::new(),
            tools: Vec::new(),
        }
    }

    /// Discover tools from every configured server. A broken server is skipped.
    pub fn discover(&mut self) -> Vec<McpTool> {
        self.tools.clear();
        for config in self.config.servers.clone() {
            if !matches!(config.transport.as_str(), "stdio" | "http") {
                continue;
            }
            let result = self.with_server(&config, |server| server.list_tools());
            let Ok(result) = result else {
                continue;
            };
            let Some(tools) = result.get("tools").and_then(Value::as_array) else {
                continue;
            };
            for tool in tools {
                let Some(name) = tool.get("name").and_then(Value::as_str) else {
                    continue;
                };
                self.tools.push(McpTool {
                    name: format!("mcp__{}__{}", config.id, name),
                    description: tool
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or("MCP tool")
                        .to_owned(),
                    input_schema: tool
                        .get("inputSchema")
                        .cloned()
                        .unwrap_or_else(|| json!({"type":"object","properties":{}})),
                    server: config.id.clone(),
                    tool: name.to_owned(),
                    read_only: config.read_only,
                });
            }
        }
        self.tools.clone()
    }

    /// Cached tools.
    #[must_use]
    pub fn tools(&self) -> &[McpTool] {
        &self.tools
    }

    /// Call one namespaced tool.
    pub fn call(&mut self, name: &str, arguments: Value) -> Result<ToolResult, McpError> {
        let tool = self
            .tools
            .iter()
            .find(|tool| tool.name == name)
            .cloned()
            .ok_or_else(|| McpError::UnknownTool(name.to_owned()))?;
        let config = self
            .config
            .servers
            .iter()
            .find(|server| server.id == tool.server)
            .cloned()
            .ok_or_else(|| McpError::UnknownServer(tool.server.clone()))?;
        let value = self.with_server(&config, |server| server.call_tool(&tool.tool, arguments))?;
        let mut content = String::new();
        if let Some(parts) = value.get("content").and_then(Value::as_array) {
            for part in parts {
                match part.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(text) = part.get("text").and_then(Value::as_str) {
                            content.push_str(text);
                            content.push('\n');
                        }
                    }
                    Some(kind) => content.push_str(&format!("[{kind} content]\n")),
                    None => {}
                }
            }
        }
        if content.is_empty() {
            content = value.to_string();
        }
        Ok(ToolResult {
            content,
            is_error: value
                .get("isError")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            diff: None,
        })
    }

    fn with_server<T>(
        &mut self,
        config: &McpServerConfig,
        operation: impl FnOnce(&mut McpServer) -> Result<T, McpError>,
    ) -> Result<T, McpError> {
        if !self.servers.contains_key(&config.id) {
            self.servers
                .insert(config.id.clone(), McpServer::start(config)?);
        }
        let result = {
            let server = self
                .servers
                .get_mut(&config.id)
                .ok_or_else(|| McpError::Message("server disappeared".to_owned()))?;
            operation(server)
        };
        if result.is_err() {
            self.servers.remove(&config.id);
        }
        result
    }
}

enum McpServer {
    Stdio(StdioServer),
    Http(HttpServer),
}

impl McpServer {
    fn start(config: &McpServerConfig) -> Result<Self, McpError> {
        if config.transport == "http" {
            let mut server = HttpServer::new(config)?;
            server.request(
                "initialize",
                json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": {"name": "thais", "version": env!("CARGO_PKG_VERSION")}
                }),
            )?;
            server.notify("notifications/initialized", json!({}))?;
            return Ok(Self::Http(server));
        }
        if config.transport != "stdio" {
            return Err(McpError::UnsupportedTransport(config.transport.clone()));
        }
        Ok(Self::Stdio(StdioServer::start(config)?))
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value, McpError> {
        match self {
            Self::Stdio(server) => server.request(method, params),
            Self::Http(server) => server.request(method, params),
        }
    }

    fn list_tools(&mut self) -> Result<Value, McpError> {
        self.request("tools/list", json!({}))
    }

    fn call_tool(&mut self, name: &str, arguments: Value) -> Result<Value, McpError> {
        self.request("tools/call", json!({"name": name, "arguments": arguments}))
    }
}

struct HttpServer {
    client: reqwest::blocking::Client,
    url: String,
    session_id: Option<String>,
    next_id: i64,
}

impl HttpServer {
    fn new(config: &McpServerConfig) -> Result<Self, McpError> {
        let url = config
            .url
            .clone()
            .ok_or_else(|| McpError::Message("HTTP MCP server requires url".to_owned()))?;
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_millis(config.timeout_ms.clamp(100, 30_000)))
            .build()
            .map_err(|error| McpError::Message(format!("HTTP MCP client: {error}")))?;
        Ok(Self {
            client,
            url,
            session_id: None,
            next_id: 1,
        })
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value, McpError> {
        let id = self.next_id;
        self.next_id += 1;
        let value = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
        let response = self.post(value)?;
        let parsed = parse_http_messages(&response)?;
        for value in parsed {
            if value.get("id").and_then(Value::as_i64) == Some(id) {
                if let Some(error) = value.get("error") {
                    return Err(McpError::Message(error.to_string()));
                }
                return Ok(value.get("result").cloned().unwrap_or(Value::Null));
            }
        }
        Err(McpError::Message(format!(
            "missing MCP response for {method}"
        )))
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<(), McpError> {
        let _ = self.post(json!({"jsonrpc":"2.0","method":method,"params":params}))?;
        Ok(())
    }

    fn post(&mut self, value: Value) -> Result<String, McpError> {
        let mut request = self
            .client
            .post(&self.url)
            .header("accept", "application/json, text/event-stream")
            .header("content-type", "application/json")
            .json(&value);
        if let Some(session_id) = &self.session_id {
            request = request.header("mcp-session-id", session_id);
        }
        let response = request
            .send()
            .map_err(|error| McpError::Message(format!("HTTP MCP: {error}")))?;
        if let Some(session_id) = response
            .headers()
            .get("mcp-session-id")
            .and_then(|value| value.to_str().ok())
        {
            self.session_id = Some(session_id.to_owned());
        }
        let status = response.status();
        let text = response
            .text()
            .map_err(|error| McpError::Message(format!("HTTP MCP body: {error}")))?;
        if !status.is_success() {
            return Err(McpError::Message(format!("HTTP MCP {status}: {text}")));
        }
        Ok(text)
    }
}

fn parse_http_messages(text: &str) -> Result<Vec<Value>, McpError> {
    if let Ok(value) = serde_json::from_str::<Value>(text) {
        return Ok(match value {
            Value::Array(values) => values,
            value => vec![value],
        });
    }
    let mut values = Vec::new();
    for line in text.lines() {
        if let Some(data) = line.strip_prefix("data:") {
            let data = data.trim();
            if data.is_empty() || data == "[DONE]" {
                continue;
            }
            if let Ok(value) = serde_json::from_str(data) {
                values.push(value);
            }
        }
    }
    if values.is_empty() {
        return Err(McpError::Message(
            "HTTP MCP returned neither JSON nor SSE data".to_owned(),
        ));
    }
    Ok(values)
}

struct StdioServer {
    child: Child,
    stdin: ChildStdin,
    receiver: Receiver<Value>,
    next_id: i64,
    timeout: Duration,
}

impl Drop for StdioServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl StdioServer {
    fn start(config: &McpServerConfig) -> Result<Self, McpError> {
        let mut child = Command::new(&config.command)
            .args(&config.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|source| McpError::Io {
                path: PathBuf::from(&config.command),
                source,
            })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| McpError::Message("MCP stdin missing".to_owned()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| McpError::Message("MCP stdout missing".to_owned()))?;
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines().map_while(Result::ok) {
                if let Ok(value) = serde_json::from_str(&line)
                    && sender.send(value).is_err()
                {
                    break;
                }
            }
        });
        let mut server = Self {
            child,
            stdin,
            receiver,
            next_id: 1,
            timeout: Duration::from_millis(config.timeout_ms.clamp(100, 30_000)),
        };
        server.request(
            "initialize",
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "thais", "version": env!("CARGO_PKG_VERSION")}
            }),
        )?;
        server.notify("notifications/initialized", json!({}))?;
        Ok(server)
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value, McpError> {
        let id = self.next_id;
        self.next_id += 1;
        self.write(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))?;
        let deadline = Instant::now() + self.timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(McpError::Timeout(method.to_owned()));
            }
            match self.receiver.recv_timeout(remaining) {
                Ok(value) => {
                    if value.get("id").and_then(Value::as_i64) == Some(id) {
                        if let Some(error) = value.get("error") {
                            return Err(McpError::Message(error.to_string()));
                        }
                        return Ok(value.get("result").cloned().unwrap_or(Value::Null));
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    return Err(McpError::Timeout(method.to_owned()));
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(McpError::Message("MCP server exited".to_owned()));
                }
            }
        }
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<(), McpError> {
        self.write(&json!({"jsonrpc":"2.0","method":method,"params":params}))
    }

    fn write(&mut self, value: &Value) -> Result<(), McpError> {
        serde_json::to_writer(&mut self.stdin, value)?;
        self.stdin
            .write_all(b"\n")
            .and_then(|()| self.stdin.flush())
            .map_err(|error| McpError::Message(format!("MCP write: {error}")))
    }
}

/// MCP failures.
#[derive(Debug, thiserror::Error)]
pub enum McpError {
    /// I/O failure.
    #[error("MCP I/O error at {path}: {source}")]
    Io {
        /// Path involved.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },
    /// Invalid config.
    #[error("invalid MCP config: {0}")]
    Toml(#[from] toml::de::Error),
    /// JSON failure.
    #[error("MCP JSON error: {0}")]
    Json(#[from] serde_json::Error),
    /// Unsupported transport.
    #[error("unsupported MCP transport: {0}")]
    UnsupportedTransport(String),
    /// Unknown namespaced tool.
    #[error("unknown MCP tool: {0}")]
    UnknownTool(String),
    /// Unknown server.
    #[error("unknown MCP server: {0}")]
    UnknownServer(String),
    /// Timeout.
    #[error("MCP timeout: {0}")]
    Timeout(String),
    /// Protocol/server error.
    #[error("MCP error: {0}")]
    Message(String),
}

fn default_timeout() -> u64 {
    5_000
}

fn default_transport() -> String {
    "stdio".to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn python_available() -> bool {
        Command::new("python3")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok()
    }

    #[test]
    fn discovers_and_calls_a_stdio_server() {
        if !python_available() {
            return;
        }
        let temp = tempfile::tempdir().expect("temp");
        let script = temp.path().join("fake_mcp.py");
        std::fs::write(
            &script,
            r#"
import json, sys
for line in sys.stdin:
    message = json.loads(line)
    method = message.get("method")
    ident = message.get("id")
    if method == "initialize":
        print(json.dumps({"jsonrpc":"2.0","id":ident,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"fake","version":"1"}}}), flush=True)
    elif method == "tools/list":
        print(json.dumps({"jsonrpc":"2.0","id":ident,"result":{"tools":[{"name":"echo","description":"Echo text","inputSchema":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}}]}}), flush=True)
    elif method == "tools/call":
        print(json.dumps({"jsonrpc":"2.0","id":ident,"result":{"content":[{"type":"text","text":"echo: " + message["params"]["arguments"]["text"]}],"isError":False}}), flush=True)
"#,
        )
        .expect("script");
        let config = McpConfig {
            servers: vec![McpServerConfig {
                id: "fake".to_owned(),
                command: "python3".to_owned(),
                url: None,
                args: vec![script.to_string_lossy().into_owned()],
                timeout_ms: 2_000,
                read_only: true,
                transport: "stdio".to_owned(),
            }],
        };
        let registry = crate::tools::ToolRegistry::read_only(temp.path())
            .expect("registry")
            .with_mcp(config.clone());
        assert!(
            registry
                .definitions()
                .iter()
                .any(|definition| definition.name == "mcp__fake__echo")
        );
        assert!(!registry.is_mutating("mcp__fake__echo"));
        let via_registry = registry
            .execute("mcp__fake__echo", json!({"text": "registry"}))
            .expect("registry call");
        assert!(via_registry.content.contains("registry"));
        let mut manager = McpManager::new(config);
        let tools = manager.discover();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "mcp__fake__echo");
        let result = manager
            .call("mcp__fake__echo", json!({"text":"hi"}))
            .expect("call");
        assert_eq!(result.content.trim(), "echo: hi");
    }

    #[test]
    fn discovers_and_calls_an_http_server() {
        use wiremock::matchers::{body_string_contains, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        let server = runtime.block_on(MockServer::start());
        runtime.block_on(async {
        Mock::given(method("POST"))
            .and(path("/mcp"))
            .and(body_string_contains("\"initialize\""))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0", "id": 1,
                "result": {"protocolVersion": "2024-11-05", "capabilities": {}, "serverInfo": {"name": "fake", "version": "1"}}
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/mcp"))
            .and(body_string_contains("notifications/initialized"))
            .respond_with(ResponseTemplate::new(202))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/mcp"))
            .and(body_string_contains("\"tools/list\""))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0", "id": 2,
                "result": {"tools": [{"name": "echo", "description": "Echo", "inputSchema": {"type": "object"}}]}
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/mcp"))
            .and(body_string_contains("\"tools/call\""))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0", "id": 3,
                "result": {"content": [{"type": "text", "text": "http echo"}], "isError": false}
            })))
            .mount(&server)
            .await;
        });

        let config = McpConfig {
            servers: vec![McpServerConfig {
                id: "fake".to_owned(),
                command: "unused".to_owned(),
                url: Some(format!("{}/mcp", server.uri())),
                args: Vec::new(),
                timeout_ms: 2_000,
                read_only: true,
                transport: "http".to_owned(),
            }],
        };
        let mut manager = McpManager::new(config);
        let tools = manager.discover();
        assert_eq!(tools.len(), 1);
        let result = manager
            .call("mcp__fake__echo", json!({}))
            .expect("http call");
        assert!(result.content.contains("http echo"));
    }
}
