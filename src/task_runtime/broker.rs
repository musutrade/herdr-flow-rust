//! One task-owned Codex connection. Dropping the last native connection before
//! a first turn discards an unpersisted fresh thread in Codex 0.163.
use super::*;
use crate::codex::rpc::Client;
mod owner;
mod transport;
use std::{
    os::unix::net::{UnixListener, UnixStream},
    time::Duration,
};

/// Serve one task-owned broker until an error or external process termination.
/// Root must contain valid runtime/journal state and the invoking process must
/// carry its task instance marker. Acquires a nonblocking exclusive broker lock,
/// recovers only attributable stale sockets, binds a socket and records ownership.
/// No normal shutdown return is promised. Runtime/lock/socket identity changes
/// fail closed; failures write best-effort status without masking the original
/// error. Drop removes only the owned socket while retaining the lock through
/// removal. Restart through the task runtime; do not blindly unlink replacements.
pub fn serve(root: &Path) -> Result<()> {
    let owner = owner::Owner::acquire(root)?;
    let result = serve_owned(root, &owner);
    if let Err(error) = &result {
        // Best effort only: a full disk must not hide the original error.
        let _ = record_failure(root, error);
    }
    result
}
fn record_failure(root: &Path, error: &FlowError) -> Result<()> {
    let _permit = crate::lifecycle::runtime_permit(root)?;
    util::atomic_write_json(
        &root.join("broker-status.json"),
        &json!({"phase":"failed", "error_kind":error.kind.type_name()}),
    )
}
fn serve_owned(root: &Path, owner: &owner::Owner) -> Result<()> {
    validate_stored_journal(root)?;
    let mut connection = None;
    loop {
        owner.verify()?;
        accept_one(root, owner, &mut connection)?;
        if let Some(client) = connection.as_mut() {
            observe(root, client)?;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn accept_one(
    root: &Path,
    owner: &owner::Owner,
    connection: &mut Option<super::native::DirectClient>,
) -> Result<()> {
    match owner.listener.accept() {
        Ok((stream, _)) => handle(root, stream, connection, owner),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn handle(
    root: &Path,
    mut stream: UnixStream,
    connection: &mut Option<super::native::DirectClient>,
    owner: &owner::Owner,
) -> Result<()> {
    let request = match transport::read(
        &mut stream,
        transport::REQUEST_LIMIT,
        Duration::from_secs(15),
    ) {
        Ok(request) => request,
        Err(_) => return Ok(()), // A malformed or abandoned frame owns only this connection.
    };
    let _permit = crate::lifecycle::runtime_permit(root)?;
    owner.verify()?;
    // Keep the action permit in this outer scope through execution and response.
    reply(root, &mut stream, connection, &request)
}

fn reply(
    root: &Path,
    stream: &mut UnixStream,
    connection: &mut Option<super::native::DirectClient>,
    request: &J,
) -> Result<()> {
    let result = match request_method(request) {
        Ok(_) => execute(root, connection, request),
        Err(error) => {
            let _ = respond(stream, Err(error), None);
            return Ok(());
        }
    };
    // Native uncertainty or journal failure stops the service, never replaying
    // an operation whose effect may already have happened.
    if let Err(error) = &result {
        if error.rpc_code.is_none() {
            return result.map(|_| ());
        }
    }
    let server_info = connection
        .as_ref()
        .and_then(|client| client.server_info.as_ref());
    let _ = respond(stream, result, server_info);
    Ok(())
}
fn respond(stream: &mut UnixStream, result: Result<J>, server_info: Option<&J>) -> Result<()> {
    let response = match result {
        Ok(value) => {
            json!({"result":value,"server_info":{"version":server_info.and_then(|info| info.get("version"))}})
        }
        Err(error) => json!({"error":{"message":error.message,"code":error.rpc_code}}),
    };
    transport::write(
        stream,
        &response,
        transport::RESPONSE_LIMIT,
        Duration::from_secs(10),
    )
}
fn request_method(request: &J) -> Result<String> {
    let method = required(request, "method")?;
    ensure(
        [
            "thread/start",
            "thread/read",
            "thread/resume",
            "thread/turns/list",
            "turn/start",
        ]
        .contains(&method.as_str()),
        "unsupported broker operation",
    )?;
    ensure(
        request["params"].is_object(),
        "broker params must be an object",
    )?;
    Ok(method)
}

fn execute(
    root: &Path,
    connection: &mut Option<super::native::DirectClient>,
    request: &J,
) -> Result<J> {
    let method = request_method(request)?;
    validate_stored_journal(root)?;
    if connection.is_none() {
        *connection = Some(super::native::direct_client(root)?);
    }
    let client = connection.as_mut().unwrap();
    super::upstream::before(root, &method, &request["params"])?;
    let result = client.call(&method, &request["params"]);
    if let Ok(value) = &result {
        super::upstream::after(root, &method, value)?;
    }
    observe(root, client)?;
    result
}
fn observe(root: &Path, client: &mut Client) -> Result<()> {
    let _permit = crate::lifecycle::runtime_permit(root)?;
    // EOF may arrive after valid frames in the same drain. Persist those
    // frames before reporting the disconnected transport to the supervisor.
    let transport = client.drain(0.001).map(|_| ());
    let events = client.pop_events();
    let mut journal = observation_journal(root)?;
    if events.is_empty() && root.join("broker-events.json").exists() {
        return transport;
    }
    // Events buffered while awaiting a response must also survive a restart.
    for event in events {
        record(&mut journal, &event)?;
        validate_journal(&journal)?;
    }
    util::atomic_write_json(&root.join("broker-events.json"), &journal)?;
    transport
}
fn observation_journal(root: &Path) -> Result<J> {
    let journal = if root.join("broker-events.json").exists() {
        util::read_state_json(root, "broker-events.json")?
    } else {
        json!({"turns":{},"pending":{}})
    };
    validate_journal(&journal)?;
    Ok(journal)
}

fn validate_stored_journal(root: &Path) -> Result<()> {
    match std::fs::symlink_metadata(root.join("broker-events.json")) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
        Ok(_) => validate_journal(&util::read_state_json(root, "broker-events.json")?),
    }
}
fn validate_journal(journal: &J) -> Result<()> {
    for field in ["turns", "pending"] {
        let map = journal[field]
            .as_object()
            .ok_or_else(|| FlowError::guard(format!("broker-events.{field} must be an object")))?;
        ensure(map.len() <= 64, "native event journal reached its bound")?;
    }
    Ok(())
}
fn record(journal: &mut J, event: &J) -> Result<()> {
    validate_journal(journal)?;
    let params = &event["params"];
    match event["method"].as_str() {
        Some("turn/started" | "turn/completed") => {
            if let Some(id) = params["turn"]["id"].as_str() {
                journal["turns"][id] = json!({"thread":params["threadId"],"turn":params["turn"]});
            }
        }
        Some(
            "item/commandExecution/requestApproval"
            | "item/fileChange/requestApproval"
            | "item/tool/requestUserInput",
        ) if !event["id"].is_null() => {
            journal["pending"][event["id"].to_string()] = event.clone();
        }
        _ => {}
    }
    Ok(())
}

pub(super) fn call(root: &Path, method: &str, params: &J) -> Result<J> {
    Ok(call_observed(root, method, params)?.result)
}

/// Metadata belongs to the same persistent connection that returned this result.
/// It is not a compatibility baseline or a version inferred from a binary path.
pub(super) struct Observation {
    pub result: J,
    pub server_version: J,
}

pub(super) fn call_observed(root: &Path, method: &str, params: &J) -> Result<Observation> {
    let mut stream = util::unix_socket::connect(&root.join("broker.sock"))?;
    transport::write(
        &mut stream,
        &json!({"method":method,"params":params}),
        transport::REQUEST_LIMIT,
        Duration::from_secs(10),
    )?;
    let value = transport::read(
        &mut stream,
        transport::RESPONSE_LIMIT,
        Duration::from_secs(30),
    )?;
    if let Some(error) = value.get("error") {
        return Err(
            FlowError::rpc(error["message"].as_str().unwrap_or("broker request failed"))
                .with_rpc(error["code"].as_i64(), None),
        );
    }
    Ok(Observation {
        result: value["result"].clone(),
        server_version: observed_version(&value["server_info"]["version"]),
    })
}

fn observed_version(value: &J) -> J {
    value
        .as_str()
        .filter(|version| !version.trim().is_empty())
        .map_or(J::Null, |version| json!(version))
}

pub(super) fn turn(root: &Path, state: &J, id: &J) -> Result<Option<J>> {
    let journal = util::read_state_json(root, "broker-events.json")?;
    validate_journal(&journal)?;
    let entry = &journal["turns"][id.as_str().unwrap_or_default()];
    if entry.is_null() {
        return Ok(None);
    }
    ensure(
        entry["thread"] == state["session"],
        "native turn event belongs to another thread",
    )?;
    Ok(Some(entry["turn"].clone()))
}

#[cfg(test)]
#[path = "../../tests/common/task_runtime_broker.rs"]
mod tests;
