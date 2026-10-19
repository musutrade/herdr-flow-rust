//! Shared error type mirroring the original Python error boundaries.
//!
//! The original project raised `QueueError`, `ProgressError`, `GuardError`,
//! `WatchError`, `AdvisorError` and a few transport errors. Keeping the kind on
//! one error type lets callers preserve those boundaries while still using the
//! `?` operator.

use std::fmt;
use std::io;

pub type Result<T> = std::result::Result<T, FlowError>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ErrorKind {
    Queue,
    Progress,
    Guard,
    AgentBusy,
    StateBusy,
    Transient,
    Watch,
    Rpc,
    RpcTimeout,
    RpcProtocol,
    Advisor,
    Timeout,
    Io,
    Json,
    Usage,
    Other,
}

const TYPE_NAMES: &[(ErrorKind, &str)] = &[
    (ErrorKind::Queue, "QueueError"),
    (ErrorKind::Progress, "ProgressError"),
    (ErrorKind::Guard, "GuardError"),
    (ErrorKind::AgentBusy, "AgentBusy"),
    (ErrorKind::StateBusy, "StateBusy"),
    (ErrorKind::Transient, "TransientError"),
    (ErrorKind::Watch, "WatchError"),
    (ErrorKind::Rpc, "RpcError"),
    (ErrorKind::RpcTimeout, "RpcTimeout"),
    (ErrorKind::RpcProtocol, "RpcProtocolError"),
    (ErrorKind::Advisor, "AdvisorError"),
    (ErrorKind::Timeout, "TimeoutExpired"),
    (ErrorKind::Io, "OSError"),
    (ErrorKind::Json, "ValueError"),
    (ErrorKind::Usage, "ValueError"),
    (ErrorKind::Other, "Error"),
];

impl ErrorKind {
    /// The Python exception name emitted by the legacy CLIs.
    pub fn type_name(self) -> &'static str {
        type_name_lookup(self)
    }
}

fn type_name_lookup(kind: ErrorKind) -> &'static str {
    TYPE_NAMES
        .iter()
        .find(|(candidate, _)| *candidate == kind)
        .map(|(_, name)| *name)
        .unwrap_or("Error")
}

#[derive(Debug)]
pub struct FlowError {
    pub kind: ErrorKind,
    pub message: String,
    /// JSON-RPC error code when the server returned one.
    pub rpc_code: Option<i64>,
    /// Raw JSON-RPC error object.
    pub rpc_error: Option<serde_json::Value>,
    pub method: Option<String>,
}

impl FlowError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            rpc_code: None,
            rpc_error: None,
            method: None,
        }
    }

    pub fn queue(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Queue, message)
    }

    pub fn progress(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Progress, message)
    }

    pub fn guard(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Guard, message)
    }

    pub fn watch(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Watch, message)
    }

    pub fn advisor(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Advisor, message)
    }

    pub fn usage(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Usage, message)
    }

    pub fn io(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Io, message)
    }

    pub fn rpc(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Rpc, message)
    }

    pub fn rpc_timeout(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::RpcTimeout, message)
    }

    pub fn rpc_protocol(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::RpcProtocol, message)
    }

    pub fn with_rpc(mut self, code: Option<i64>, error: Option<serde_json::Value>) -> Self {
        self.rpc_code = code;
        self.rpc_error = error;
        self
    }

    pub fn with_method(mut self, method: impl Into<String>) -> Self {
        self.method = Some(method.into());
        self
    }

    pub fn is(&self, kind: ErrorKind) -> bool {
        self.kind == kind
    }

    /// A stable, credential-neutral category for persisted diagnostics.
    pub fn category(&self) -> String {
        match self.kind {
            ErrorKind::Io | ErrorKind::Json => self.message.clone(),
            _ => self.message.clone(),
        }
    }
}

impl fmt::Display for FlowError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for FlowError {}

impl From<io::Error> for FlowError {
    fn from(value: io::Error) -> Self {
        Self::new(ErrorKind::Io, value.to_string())
    }
}

impl From<serde_json::Error> for FlowError {
    fn from(value: serde_json::Error) -> Self {
        Self::new(ErrorKind::Json, value.to_string())
    }
}

impl From<std::string::FromUtf8Error> for FlowError {
    fn from(value: std::string::FromUtf8Error) -> Self {
        Self::new(ErrorKind::Json, value.to_string())
    }
}
