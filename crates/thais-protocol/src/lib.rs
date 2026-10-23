#![forbid(unsafe_code)]

//! Wire types for the Thais Agent API: JSON-RPC 2.0 envelopes, session
//! methods, and the event stream shared by every client.

pub mod error;
pub mod event;
pub mod jsonrpc;
pub mod method;
pub mod session;

/// Version of the Thais Agent API carried in `initialize`.
pub const PROTOCOL_VERSION: &str = "1.0";

/// JSON-RPC version required on every envelope.
pub const JSONRPC_VERSION: &str = "2.0";
