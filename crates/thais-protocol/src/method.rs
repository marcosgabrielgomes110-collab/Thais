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

/// List sessions.
pub const SESSION_LIST: &str = "session.list";
/// Get one session.
pub const SESSION_GET: &str = "session.get";
/// Rename a session.
pub const SESSION_RENAME: &str = "session.rename";
/// Archive a session.
pub const SESSION_ARCHIVE: &str = "session.archive";
/// Soft-delete a session.
pub const SESSION_DELETE: &str = "session.delete";
/// Resume a session.
pub const SESSION_RESUME: &str = "session.resume";
/// Change a session model role.
pub const SESSION_SET_MODEL: &str = "session.set_model";
/// List model catalog entries.
pub const MODELS_LIST: &str = "models.list";

/// Search the session Event Log with FTS5.
pub const HISTORY_SEARCH: &str = "history.search";
