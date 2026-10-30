use super::Error;
use serde_json::{json, Value};
use std::io::Write;

pub(super) type CliResult<T> = Result<T, Failure>;
pub(super) enum Failure {
    ObserverBusy,
    Usage(String),
    Output(std::io::Error),
    Store(Error),
}
impl Failure {
    pub fn usage(message: impl Into<String>) -> Self {
        Self::Usage(message.into())
    }
}
impl From<Error> for Failure {
    fn from(error: Error) -> Self {
        Self::Store(error)
    }
}
impl From<std::io::Error> for Failure {
    fn from(error: std::io::Error) -> Self {
        Self::Store(Error::Io(error))
    }
}

pub(super) fn write_success(data: Value) -> std::io::Result<()> {
    let line = json!({"ok":true,"data":data}).to_string();
    writeln!(std::io::stdout().lock(), "{line}")
}
pub(super) fn success(data: Value) -> i32 {
    match write_success(data) {
        Ok(()) => 0,
        Err(error) => failure(Failure::Output(error)),
    }
}
pub(super) fn failure(failure: Failure) -> i32 {
    let (code, detail) = describe(failure);
    let _ = writeln!(
        std::io::stderr().lock(),
        "{}",
        json!({"ok":false,"error":detail})
    );
    code
}
fn detail(code: &str, message: impl ToString) -> Value {
    json!({"code":code,"message":message.to_string()})
}
fn describe(failure: Failure) -> (i32, Value) {
    match failure {
        Failure::ObserverBusy => (
            5,
            detail("busy", "observation already active for this binding"),
        ),
        Failure::Output(error) => (6, detail("output_unavailable", error)),
        Failure::Usage(message) => (2, detail("usage", message)),
        Failure::Store(error) => storage(error),
    }
}
fn storage(error: Error) -> (i32, Value) {
    let message = error.to_string();
    match error {
        Error::Invalid(_) => (2, detail("invalid_input", message)),
        Error::NotFound(_) => (3, detail("not_found", message)),
        Error::RequestConflict => (4, detail("request_conflict", message)),
        Error::RevisionConflict { expected, actual } => (
            4,
            json!({"code":"revision_conflict","message":message,"expected":expected,"actual":actual}),
        ),
        Error::UnsupportedSchema(version) => (
            6,
            json!({"code":"unsupported_schema","message":message,"version":version}),
        ),
        Error::Sqlite(error) => sqlite(error),
        _ => (6, detail("storage_unavailable", message)),
    }
}
fn sqlite(error: rusqlite::Error) -> (i32, Value) {
    use rusqlite::ErrorCode;
    let (exit, code) = match error.sqlite_error_code() {
        Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked) => (5, "busy"),
        Some(ErrorCode::ReadOnly) => (6, "read_only"),
        Some(ErrorCode::DiskFull) => (6, "storage_full"),
        _ => (6, "storage_unavailable"),
    };
    (exit, detail(code, error))
}
