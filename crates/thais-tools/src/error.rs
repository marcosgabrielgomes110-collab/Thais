//! Tool errors.

use std::path::PathBuf;

use thiserror::Error;

/// Tool execution failures.
#[derive(Debug, Error)]
pub enum ToolError {
    /// The path escapes the workspace.
    #[error("path escapes the workspace: {0}")]
    Escape(PathBuf),
    /// The path does not exist.
    #[error("path not found: {0}")]
    NotFound(PathBuf),
    /// I/O failure.
    #[error("I/O error at {path}: {source}")]
    Io {
        /// Path involved.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },
    /// Invalid tool arguments.
    #[error("invalid arguments: {0}")]
    InvalidArgs(String),
    /// Unknown tool.
    #[error("unknown tool: {0}")]
    UnknownTool(String),
    /// The gate denied the action.
    #[error("tool denied: {0}")]
    Denied(String),
    /// A binary file was requested as text.
    #[error("binary file is not readable as text: {0}")]
    Binary(PathBuf),
}
