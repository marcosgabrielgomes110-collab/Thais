#![forbid(unsafe_code)]

//! Tool definitions and execution. M0 only provides the boundary; real tools
//! arrive in M1 and M2.

/// Version of the internal tool API.
pub const TOOL_API_VERSION: &str = "0.1";
