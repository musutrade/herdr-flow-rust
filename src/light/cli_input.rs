use super::{
    cli_args::Command,
    cli_output::{CliResult, Failure},
    Change, Request,
};
use serde_json::Value;
use std::{fs::OpenOptions, io::Read, os::unix::fs::OpenOptionsExt};

pub(super) fn request(path: &str, command: Command) -> CliResult<Request> {
    let bytes = read(path)?;
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|e| Failure::usage(format!("invalid JSON input: {e}")))?;
    let request: Request = serde_json::from_slice(&bytes)
        .map_err(|e| Failure::usage(format!("invalid Request: {e}")))?;
    let known = serde_json::to_value(&request).map_err(|e| Failure::usage(e.to_string()))?;
    known_fields(&value, &known)?;
    let matches = matches!(
        (command, &request.change),
        (Command::Add, Change::CreateTask { .. })
            | (Command::Report, Change::Report { .. })
            | (Command::Attach, Change::AttachBinding { .. })
    );
    if !matches {
        return Err(Failure::usage("change.kind does not match the CLI command"));
    }
    Ok(request)
}
fn read(path: &str) -> CliResult<Vec<u8>> {
    if path == "-" {
        return bounded(std::io::stdin().lock());
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(Failure::usage("input file must be a regular file"));
    }
    bounded(file)
}
fn bounded(reader: impl Read) -> CliResult<Vec<u8>> {
    let mut bytes = Vec::new();
    reader.take(65537).read_to_end(&mut bytes)?;
    if bytes.len() > 65536 {
        return Err(Failure::usage("input exceeds 65536 bytes"));
    }
    Ok(bytes)
}
fn known_fields(input: &Value, known: &Value) -> CliResult<()> {
    match (input, known) {
        (Value::Object(input), Value::Object(known)) => object_fields(input, known),
        (Value::Array(input), Value::Array(known)) => {
            for (a, b) in input.iter().zip(known) {
                known_fields(a, b)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}
fn object_fields(
    input: &serde_json::Map<String, Value>,
    known: &serde_json::Map<String, Value>,
) -> CliResult<()> {
    for (key, value) in input {
        let expected = known
            .get(key)
            .ok_or_else(|| Failure::usage(format!("unknown JSON field {key}")))?;
        known_fields(value, expected)?;
    }
    Ok(())
}
