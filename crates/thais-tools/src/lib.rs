#![forbid(unsafe_code)]

//! Read-only tools and the M1 permission policy.

pub mod bash;
pub mod error;
pub mod git;
pub mod memory;
pub mod mutating;
pub mod path_jail;
pub mod permission;
pub mod tools;

pub use bash::BashTool;
pub use error::ToolError;
pub use git::{GitDiffTool, GitStatusTool};
pub use mutating::{ApplyPatchTool, EditTool, WriteTool};
pub use path_jail::PathJail;
pub use permission::{PermissionDecision, PermissionGate, PolicyMode};
pub use tools::{Tool, ToolDefinition, ToolRegistry, ToolResult};

/// Version of the internal tool API.
pub const TOOL_API_VERSION: &str = "0.1";
