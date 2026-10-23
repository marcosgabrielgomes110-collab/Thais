//! JSON-RPC 2.0 envelopes used by every Agent API transport.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::JSONRPC_VERSION;
use crate::error::{ErrorObject, ProtocolError};

/// JSON-RPC request identifier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Id {
    /// Numeric identifier.
    Number(i64),
    /// String identifier.
    String(String),
    /// Null identifier, used by error responses to unparseable requests.
    Null,
}

impl From<i64> for Id {
    fn from(value: i64) -> Self {
        Self::Number(value)
    }
}

impl From<String> for Id {
    fn from(value: String) -> Self {
        Self::String(value)
    }
}

impl From<&str> for Id {
    fn from(value: &str) -> Self {
        Self::String(value.to_owned())
    }
}

/// A JSON-RPC request: expects exactly one response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    /// Must be `2.0`.
    pub jsonrpc: String,
    /// Request identifier.
    pub id: Id,
    /// Method name.
    pub method: String,
    /// Method parameters. Defaults to `null` when omitted.
    #[serde(default)]
    pub params: Value,
}

impl Request {
    /// Build a request.
    #[must_use]
    pub fn new(id: impl Into<Id>, method: impl Into<String>, params: Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_owned(),
            id: id.into(),
            method: method.into(),
            params,
        }
    }

    /// Validate the envelope version and method name.
    pub fn validate(&self) -> Result<(), ProtocolError> {
        if self.jsonrpc != JSONRPC_VERSION {
            return Err(ProtocolError::InvalidVersion(self.jsonrpc.clone()));
        }
        if self.method.is_empty() {
            return Err(ProtocolError::InvalidRequest(
                "method must not be empty".to_owned(),
            ));
        }
        Ok(())
    }

    /// Deserialize the parameters as a typed value.
    pub fn params_as<T: DeserializeOwned>(&self) -> Result<T, ProtocolError> {
        serde_json::from_value(self.params.clone()).map_err(ProtocolError::Json)
    }
}

/// A JSON-RPC notification: no response is expected.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Notification {
    /// Must be `2.0`.
    pub jsonrpc: String,
    /// Method name.
    pub method: String,
    /// Method parameters. Defaults to `null` when omitted.
    #[serde(default)]
    pub params: Value,
}

impl Notification {
    /// Build a notification.
    #[must_use]
    pub fn new(method: impl Into<String>, params: Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_owned(),
            method: method.into(),
            params,
        }
    }

    /// Validate the envelope version and method name.
    pub fn validate(&self) -> Result<(), ProtocolError> {
        if self.jsonrpc != JSONRPC_VERSION {
            return Err(ProtocolError::InvalidVersion(self.jsonrpc.clone()));
        }
        if self.method.is_empty() {
            return Err(ProtocolError::InvalidRequest(
                "method must not be empty".to_owned(),
            ));
        }
        Ok(())
    }

    /// Deserialize the parameters as a typed value.
    pub fn params_as<T: DeserializeOwned>(&self) -> Result<T, ProtocolError> {
        serde_json::from_value(self.params.clone()).map_err(ProtocolError::Json)
    }
}

/// A JSON-RPC response: carries either `result` or `error`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    /// Must be `2.0`.
    pub jsonrpc: String,
    /// Identifier of the request being answered.
    pub id: Id,
    /// Successful result.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// Error result.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorObject>,
}

impl Response {
    /// Build a successful response.
    #[must_use]
    pub fn success(id: impl Into<Id>, result: impl Into<Value>) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_owned(),
            id: id.into(),
            result: Some(result.into()),
            error: None,
        }
    }

    /// Build an error response.
    #[must_use]
    pub fn error(id: impl Into<Id>, error: ErrorObject) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_owned(),
            id: id.into(),
            result: None,
            error: Some(error),
        }
    }

    /// Validate the envelope and ensure exactly one of `result`/`error` is set.
    pub fn validate(&self) -> Result<(), ProtocolError> {
        if self.jsonrpc != JSONRPC_VERSION {
            return Err(ProtocolError::InvalidVersion(self.jsonrpc.clone()));
        }
        if self.result.is_some() == self.error.is_some() {
            return Err(ProtocolError::InvalidRequest(
                "response must carry exactly one of result or error".to_owned(),
            ));
        }
        Ok(())
    }

    /// Deserialize the result as a typed value.
    pub fn result_as<T: DeserializeOwned>(&self) -> Result<T, ProtocolError> {
        let value = self
            .result
            .clone()
            .ok_or_else(|| ProtocolError::InvalidRequest("response has no result".to_owned()))?;
        serde_json::from_value(value).map_err(ProtocolError::Json)
    }
}

/// Any JSON-RPC message.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Message {
    /// A request.
    Request(Request),
    /// A notification.
    Notification(Notification),
    /// A response.
    Response(Response),
}

impl Message {
    /// Validate the contained message.
    pub fn validate(&self) -> Result<(), ProtocolError> {
        match self {
            Self::Request(request) => request.validate(),
            Self::Notification(notification) => notification.validate(),
            Self::Response(response) => response.validate(),
        }
    }
}
