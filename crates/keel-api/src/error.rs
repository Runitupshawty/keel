//! Typed API errors. `code` is a JSON-RPC 2.0 error code: the reserved ones for protocol
//! errors, `-320xx` for operation errors; `data` carries structured detail (a fresh
//! preview for `PLAN_CHANGED`).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ApiError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl ApiError {
    /// Not JSON.
    pub const PARSE: i64 = -32700;
    /// Not a JSON-RPC 2.0 request (or one over the size limit).
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    pub const INTERNAL: i64 = -32603;
    /// The operation ran and failed (I/O, a locked library, a refused request).
    pub const FAILED: i64 = -32000;
    /// A path, source, tag, job, peer or grant that does not exist.
    pub const NOT_FOUND: i64 = -32001;
    /// No such plan, or it expired (plans live `plans::TTL`).
    pub const PLAN_EXPIRED: i64 = -32002;
    /// `execute` got an input hash other than the previewed one.
    pub const PLAN_MISMATCH: i64 = -32003;
    /// The sources changed since the preview; `data` is the fresh preview to confirm.
    pub const PLAN_CHANGED: i64 = -32004;
    /// Devices and shares need keel-net, which is off in this host's settings.
    pub const NET_DISABLED: i64 = -32005;
    /// The request did not finish within the host's per-request timeout.
    pub const TIMEOUT: i64 = -32006;
    /// A browser connection (keel-daemon `--web`) sent something before `auth`, or a
    /// wrong token.
    pub const UNAUTHORIZED: i64 = -32007;

    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    pub fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }

    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::new(Self::INVALID_PARAMS, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(Self::NOT_FOUND, message)
    }

    pub fn failed(message: impl Into<String>) -> Self {
        Self::new(Self::FAILED, message)
    }

    /// The JSON-RPC `error` object.
    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or_else(|_| serde_json::json!({"code": self.code}))
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.message, self.code)
    }
}

impl std::error::Error for ApiError {}

/// keel-core and keel-net errors: `FAILED` with the whole context chain.
impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        Self::failed(format!("{e:#}"))
    }
}

pub type Result<T, E = ApiError> = std::result::Result<T, E>;
