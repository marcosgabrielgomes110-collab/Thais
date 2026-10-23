//! Protocol errors and their JSON-RPC error-object representation.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// JSON-RPC parse error.
pub const PARSE_ERROR: i64 = -32700;
/// JSON-RPC invalid request.
pub const INVALID_REQUEST: i64 = -32600;
/// JSON-RPC method not found.
pub const METHOD_NOT_FOUND: i64 = -32601;
/// JSON-RPC invalid params.
pub const INVALID_PARAMS: i64 = -32602;
/// JSON-RPC internal error.
pub const INTERNAL_ERROR: i64 = -32603;

/// The `error` member of a JSON-RPC response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorObject {
    /// Numeric error code.
    pub code: i64,
    /// Short human-readable message.
    pub message: String,
    /// Optional structured details.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl ErrorObject {
    /// Build an error object with no structured data.
    #[must_use]
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    /// Attach structured data to this error.
    #[must_use]
    pub fn with_data(mut self, data: serde_json::Value) -> Self {
        self.data = Some(data);
        self
    }
}

/// Errors raised while validating or interpreting protocol messages.
#[derive(Debug, Error)]
pub enum ProtocolError {
    /// The envelope is not valid JSON.
    #[error("invalid JSON: {0}")]
    Json(#[from] serde_json::Error),
    /// The `jsonrpc` member is missing or not `2.0`.
    #[error("invalid JSON-RPC version: {0}")]
    InvalidVersion(String),
    /// The request shape is invalid.
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    /// The requested method is unknown.
    #[error("method not found: {0}")]
    MethodNotFound(String),
    /// The method parameters are invalid.
    #[error("invalid params: {0}")]
    InvalidParams(String),
    /// An internal failure occurred.
    #[error("internal error: {0}")]
    Internal(String),
}

impl ProtocolError {
    /// Return the JSON-RPC error code for this error.
    #[must_use]
    pub fn code(&self) -> i64 {
        match self {
            Self::Json(_) => PARSE_ERROR,
            Self::InvalidVersion(_) | Self::InvalidRequest(_) => INVALID_REQUEST,
            Self::MethodNotFound(_) => METHOD_NOT_FOUND,
            Self::InvalidParams(_) => INVALID_PARAMS,
            Self::Internal(_) => INTERNAL_ERROR,
        }
    }

    /// Convert this error into a JSON-RPC error object.
    #[must_use]
    pub fn to_error_object(&self) -> ErrorObject {
        ErrorObject::new(self.code(), self.to_string())
    }
}
