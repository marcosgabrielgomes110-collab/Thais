//! Method names exposed by the Agent API.

/// Handshake and capability negotiation.
pub const INITIALIZE: &str = "initialize";
/// Create a session.
pub const SESSION_CREATE: &str = "session.create";
/// Send input to a session.
pub const SESSION_SEND: &str = "session.send";
/// Cancel an in-flight turn.
pub const SESSION_CANCEL: &str = "session.cancel";
/// Approve or deny a pending action.
pub const SESSION_APPROVE: &str = "session.approve";
/// Subscribe to a session's event stream.
pub const SESSION_SUBSCRIBE: &str = "session.subscribe";
/// Notification carrying one session event.
pub const SESSION_EVENT: &str = "session.event";
