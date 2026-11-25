//! JSON-RPC 2.0 messages, one per line (see PROTOCOL.md).

use serde::{Deserialize, Serialize};
use serde_json::{json, Value as Json};

/// Bumped when a method or a result changes incompatibly (new methods and fields don't bump it).
pub const PROTOCOL_VERSION: u32 = 1;

pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const INTERNAL_ERROR: i64 = -32603;
/// A dbear error: `data` is the serialized `dbcore::Error` (`{"kind": …, "message": …}`).
pub const DBEAR_ERROR: i64 = -32000;

/// Any message a peer can send: request (method + id), notification (method, no id) or response.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Incoming {
    pub jsonrpc: Option<String>,
    /// Kept apart from "absent" so `"id": null` is still a request.
    #[serde(deserialize_with = "present")]
    pub id: Option<Json>,
    pub method: Option<String>,
    pub params: Option<Json>,
    pub result: Option<Json>,
    pub error: Option<RpcError>,
}

fn present<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Json>, D::Error> {
    Json::deserialize(d).map(Some)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Json>,
}

impl RpcError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self { code, message: message.into(), data: None }
    }

    pub fn invalid_params(message: impl std::fmt::Display) -> Self {
        let error = dbcore::Error::InvalidConfig(format!("Invalid params: {message}"));
        Self { code: INVALID_PARAMS, message: error.to_string(), data: serde_json::to_value(&error).ok() }
    }

    /// The dbcore error behind it (dbear errors and invalid params carry one).
    pub fn core(&self) -> Option<dbcore::Error> {
        self.data.clone().and_then(|d| serde_json::from_value(d).ok())
    }

    pub fn is_cancelled(&self) -> bool {
        matches!(self.core(), Some(dbcore::Error::Cancelled))
    }
}

impl From<dbcore::Error> for RpcError {
    fn from(error: dbcore::Error) -> Self {
        Self { code: DBEAR_ERROR, message: error.to_string(), data: serde_json::to_value(&error).ok() }
    }
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

pub fn request(id: &Json, method: &str, params: &Json) -> String {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string()
}

pub fn notification(method: &str, params: &Json) -> String {
    json!({"jsonrpc": "2.0", "method": method, "params": params}).to_string()
}

pub fn response(id: &Json, outcome: Result<Json, RpcError>) -> String {
    match outcome {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err(error) => json!({"jsonrpc": "2.0", "id": id, "error": error}),
    }
    .to_string()
}
