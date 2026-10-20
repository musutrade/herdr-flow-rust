//! One task-owned Codex connection. Dropping the last native connection before
//! a first turn discards an unpersisted fresh thread in Codex 0.163.
use super::*;
use crate::codex::rpc::Client;
use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::{UnixListener, UnixStream},
    time::Duration,
};

pub fn serve(root: &Path) -> Result<()> {
    let listener = prepare_listener(root)?;
    let mut connection = None;
    loop {
        match listener.accept() {
            Ok((stream, _)) => handle(root, stream, &mut connection)?,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(error.into()),
        }
        if let Some(client) = connection.as_mut() {
            observe(root, client)?;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}
fn prepare_listener(root: &Path) -> Result<UnixListener> {
    let runtime = util::read_state_json(root, "runtime.json")?;
    ensure(
        std::env::var(crate::task_env::MARKER_ENV).ok().as_deref() == runtime["instance"].as_str(),
        "broker requires its task environment identity",
    )?;
    let path = root.join("broker.sock");
    ensure(
        !path.exists(),
        "broker socket already exists; inspect previous process",
    )?;
    let listener = UnixListener::bind(&path)?;
    listener.set_nonblocking(true)?;
    Ok(listener)
}

fn handle(
    root: &Path,
    mut stream: UnixStream,
    connection: &mut Option<super::native::DirectClient>,
) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(15)))?;
    let mut line = String::new();
    BufReader::new(&stream).read_line(&mut line)?;
    ensure(line.len() <= 1024 * 1024, "broker request too large")?;
    let request: J = serde_json::from_str(&line)?;
    let result = execute(root, connection, &request);
    let response = match result {
        Ok(value) => json!({"result":value}),
        Err(error) => json!({"error":{"message":error.message,"code":error.rpc_code}}),
    };
    writeln!(stream, "{response}")?;
    Ok(())
}
fn execute(
    root: &Path,
    connection: &mut Option<super::native::DirectClient>,
    request: &J,
) -> Result<J> {
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
    if connection.is_none() {
        *connection = Some(super::native::direct_client(root)?);
    }
    let client = connection.as_mut().unwrap();
    let result = client.call(&method, &request["params"]);
    observe(root, client)?;
    result
}
fn observe(root: &Path, client: &mut Client) -> Result<()> {
    // EOF may arrive after valid frames in the same drain. Persist those
    // frames before reporting the disconnected transport to the supervisor.
    let transport = client.drain(0.001).map(|_| ());
    let events = client.pop_events();
    if events.is_empty() && root.join("broker-events.json").exists() {
        return transport;
    }
    let mut journal = if root.join("broker-events.json").exists() {
        util::read_state_json(root, "broker-events.json")?
    } else {
        json!({"turns":{},"pending":{}})
    };
    // Events buffered while awaiting a response must also survive a restart.
    for event in events {
        record(&mut journal, &event);
    }
    ensure(
        journal["turns"].as_object().unwrap().len() <= 64
            && journal["pending"].as_object().unwrap().len() <= 64,
        "native event journal reached its bound",
    )?;
    util::atomic_write_json(&root.join("broker-events.json"), &journal)?;
    transport
}
fn record(journal: &mut J, event: &J) {
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
}

pub(super) fn call(root: &Path, method: &str, params: &J) -> Result<J> {
    let mut stream = UnixStream::connect(root.join("broker.sock"))?;
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    writeln!(stream, "{}", json!({"method":method,"params":params}))?;
    let mut text = String::new();
    BufReader::new(stream).read_line(&mut text)?;
    let value: J = serde_json::from_str(&text)?;
    if let Some(error) = value.get("error") {
        return Err(
            FlowError::rpc(error["message"].as_str().unwrap_or("broker request failed"))
                .with_rpc(error["code"].as_i64(), None),
        );
    }
    Ok(value["result"].clone())
}

pub(super) fn turn(root: &Path, state: &J, id: &J) -> Result<Option<J>> {
    let journal = util::read_state_json(root, "broker-events.json")?;
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
