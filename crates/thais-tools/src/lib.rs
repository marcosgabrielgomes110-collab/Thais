#![forbid(unsafe_code)]

//! Read-only tools and the M1 permission policy.

pub mod error;
pub mod path_jail;
pub mod permission;
pub mod tools;

pub use error::ToolError;
pub use path_jail::PathJail;
pub use permission::{PermissionDecision, PermissionGate};
pub use tools::{Tool, ToolDefinition, ToolRegistry, ToolResult};

/// Version of the internal tool API.
pub const TOOL_API_VERSION: &str = "0.1";
