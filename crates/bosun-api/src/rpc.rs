//! The JSON-RPC 2.0 envelope (#4).
//!
//! On the pipe or socket every message is one JSON object on one line, ended by `\n`; a line
//! holds no raw newline because JSON escapes it. A request with an `id` gets exactly one
//! response line with the same `id`; a request without one is a notification and gets none.
//! Batches (JSON arrays) are not supported and are answered with [`code::INVALID_REQUEST`].
//!
//! The envelope is the specification's, not bosun's, so it is not in the generated schema;
//! the `params` and `result` of each method are.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The value of the `jsonrpc` member.
pub const JSONRPC: &str = "2.0";

/// The longest line, in bytes, a peer accepts. A longer line ends the connection.
pub const MAX_LINE: usize = 16 << 20;

/// Error codes. The negative range below -32000 is the specification's; bosun's own codes are
/// in its server-error range.
pub mod code {
    /// The line is not JSON.
    pub const PARSE_ERROR: i32 = -32700;
    /// The JSON is not a request object (a batch is not one either).
    pub const INVALID_REQUEST: i32 = -32600;
    /// No method of that name.
    pub const METHOD_NOT_FOUND: i32 = -32601;
    /// The params do not match the method.
    pub const INVALID_PARAMS: i32 = -32602;
    /// The daemon failed in a way the request did not cause.
    pub const INTERNAL_ERROR: i32 = -32603;
    /// The request names an identity the daemon does not serve.
    pub const UNKNOWN_IDENTITY: i32 = -32001;
    /// The identity's store or ingest failed to do what was asked.
    pub const FAILED: i32 = -32002;
    /// The request's agent credential does not admit it: missing, unknown or revoked, bound to
    /// another item, or presented to a method outside `agent.*` (#63).
    pub const UNAUTHORIZED: i32 = -32003;
    /// The agent's tool is over its per-attempt cap; the call is recorded as refused (#63).
    pub const RATE_CAPPED: i32 = -32004;
    /// The item, session or card the request names is not known (#70).
    pub const NOT_FOUND: i32 = -32005;
}

/// A request or notification.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub jsonrpc: String,
    /// Absent for a notification. bosun's client sends integers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
    pub method: String,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub params: Value,
}

/// The response to a request: exactly one of `result` and `error`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub jsonrpc: String,
    /// The request's id; `null` when it could not be read.
    pub id: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorObject>,
}

/// The `error` member of a response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErrorObject {
    /// One of [`code`].
    pub code: i32,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl ErrorObject {
    #[must_use]
    pub fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }
}

impl Response {
    #[must_use]
    pub fn result(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: JSONRPC.to_owned(),
            id,
            result: Some(result),
            error: None,
        }
    }

    #[must_use]
    pub fn error(id: Value, error: ErrorObject) -> Self {
        Self {
            jsonrpc: JSONRPC.to_owned(),
            id,
            result: None,
            error: Some(error),
        }
    }
}
