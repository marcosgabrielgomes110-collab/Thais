#![forbid(unsafe_code)]

//! Read-only tools and the M1 permission policy.

pub mod bash;
pub mod error;
pub mod filesystem;
pub mod git;
pub mod lsp;
pub mod mcp;
pub mod memory;
pub mod mutating;
pub mod path_jail;
pub mod permission;
pub mod sandbox;
pub mod secret;
pub mod subagent;
pub mod tools;

pub use bash::BashTool;
pub use error::ToolError;
pub use filesystem::{DeleteTool, MkdirTool, MoveTool};
pub use git::{GitCommitTool, GitDiffTool, GitLogTool, GitShowTool, GitStatusTool};
pub use lsp::{LspConfig, LspError, LspManager, LspReport};
pub use mcp::{McpConfig, McpError, McpManager, McpServerConfig, McpTool};
pub use memory::{MEMORY_FILE, MemoryReadTool, MemorySearchTool, MemoryWriteTool};
pub use mutating::{ApplyPatchTool, EditTool, WriteTool};
pub use path_jail::PathJail;
pub use permission::{PermissionDecision, PermissionGate, PolicyMode};
pub use sandbox::{Sandbox, SandboxBackend, SandboxMode};
pub use secret::{SecretFinding, SecretScanTool};
pub use subagent::SubagentTool;
pub use tools::{Tool, ToolDefinition, ToolRegistry, ToolResult};

/// Version of the internal tool API.
pub const TOOL_API_VERSION: &str = "0.1";
