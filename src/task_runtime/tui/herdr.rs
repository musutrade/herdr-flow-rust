//! Bounded Herdr API calls, pinned to the server and terminal we created.
use super::*;
use std::io::{BufRead, BufReader, Read, Write};
use std::time::Duration;

pub(super) fn call(binding: &J, method: &str, params: J) -> Result<J> {
    let socket = &binding["herdr_socket"];
    let path = Path::new(socket["path"].as_str().unwrap_or_default());
    ensure(
        upstream::socket_identity(path)? == *socket,
        "Herdr socket changed",
    )?;
    let mut stream = util::unix_socket::connect(path)?;
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    stream.set_write_timeout(Some(Duration::from_secs(3)))?;
    let id = util::pyjson::uuid_hex();
    let mut bytes = serde_json::to_vec(&json!({"id":id,"method":method,"params":params}))?;
    bytes.push(b'\n');
    stream.write_all(&bytes)?;
    response(stream, &id)
}

fn response(stream: std::os::unix::net::UnixStream, id: &str) -> Result<J> {
    let mut response = Vec::new();
    BufReader::new(stream)
        .take(1024 * 1024 + 1)
        .read_until(b'\n', &mut response)?;
    ensure(
        response.len() <= 1024 * 1024,
        "Herdr response exceeds bound",
    )?;
    let value: J = serde_json::from_slice(&response)?;
    ensure(value["id"] == id, "Herdr response identity changed")?;
    if !value["error"].is_null() {
        return Err(FlowError::guard(format!(
            "Herdr {}: {}",
            value["error"]["code"], value["error"]["message"]
        )));
    }
    Ok(value["result"].clone())
}

pub(super) fn pane(binding: &J) -> Result<Option<J>> {
    match call(binding, "pane.get", json!({"pane_id":binding["pane"]})) {
        Ok(value) => {
            verify_pane(binding, &value["pane"])?;
            Ok(Some(value["pane"].clone()))
        }
        Err(error) if error.message.starts_with("Herdr \"pane_not_found\":") => Ok(None),
        Err(error) => Err(error),
    }
}

fn verify_pane(binding: &J, pane: &J) -> Result<()> {
    ensure(
        pane["terminal_id"] == binding["terminal"],
        "Herdr pane terminal changed; refusing input/close",
    )?;
    if let Some(session) = pane["agent_session"].get("value") {
        ensure(
            *session == binding["session"],
            "Herdr pane now belongs to another session; refusing input/close",
        )?;
    }
    Ok(())
}

pub(super) fn run(binding: &J, command: String) -> Result<()> {
    ensure(pane(binding)?.is_some(), "Herdr pane disappeared")?;
    // Use the native command admission path, including its idle-shell and
    // canonical line-length checks. A timeout is never followed by resubmission.
    let options = util::process::CommandOptions {
        timeout: Duration::from_secs(5),
        env: [(
            "HERDR_SOCKET_PATH".to_string(),
            required(&binding["herdr_socket"], "path")?,
        )]
        .into(),
        ..Default::default()
    };
    let output = util::process::run_command(
        &[
            "herdr".into(),
            "pane".into(),
            "run".into(),
            required(binding, "pane")?,
            command,
        ],
        &options,
    )?;
    ensure(
        output.success(),
        &format!("Herdr launch failed: {}", output.stderr_text()),
    )
}

pub(super) fn report(binding: &J) -> Result<()> {
    ensure(pane(binding)?.is_some(), "Herdr pane disappeared")?;
    call(
        binding,
        "pane.report_agent_session",
        json!({
            "pane_id":binding["pane"],"source":"herdr:codex","agent":"codex",
            "agent_session_id":binding["session"],"session_start_source":"resume"
        }),
    )?;
    let observed =
        pane(binding)?.ok_or_else(|| FlowError::guard("Herdr pane disappeared after report"))?;
    ensure(
        observed["agent_session"]["value"] == binding["session"],
        "Herdr did not bind the native session",
    )?;
    Ok(())
}

pub(super) fn agent_ready(binding: &J) -> Result<bool> {
    match call(binding, "agent.get", json!({"target":binding["pane"]})) {
        Ok(value) => {
            let agent = &value["agent"];
            verify_pane(binding, agent)?;
            ensure(
                agent["agent_session"]["value"] == binding["session"] && agent["agent"] == "codex",
                "Herdr agent identity changed",
            )?;
            Ok(matches!(
                agent["agent_status"].as_str(),
                Some("working" | "idle" | "done" | "blocked")
            ))
        }
        Err(error) if error.message.starts_with("Herdr \"agent_not_found\":") => Ok(false),
        Err(error) => Err(error),
    }
}

pub(super) fn close(binding: &mut J) -> Result<()> {
    if binding["phase"] == "closed" {
        return Ok(());
    }
    ensure(
        !binding["pane"].is_null() && !binding["terminal"].is_null(),
        "pane creation uncertain; inspect without replay",
    )?;
    close_present(binding)?;
    ensure(pane(binding)?.is_none(), "Herdr pane still exists")?;
    binding["phase"] = json!("closed");
    Ok(())
}

fn close_present(binding: &J) -> Result<()> {
    if pane(binding)?.is_some() {
        let processes = call(
            binding,
            "pane.process_info",
            json!({"pane_id":binding["pane"]}),
        )?;
        safe_foreground(binding, &processes["process_info"])?;
        call(binding, "pane.close", json!({"pane_id":binding["pane"]}))?;
    }
    Ok(())
}

fn safe_foreground(binding: &J, processes: &J) -> Result<()> {
    let shell = processes["shell_pid"]
        .as_u64()
        .ok_or_else(|| FlowError::guard("Herdr shell identity missing"))?;
    let foreground = processes["foreground_processes"]
        .as_array()
        .ok_or_else(|| FlowError::guard("Herdr process observation missing"))?;
    for process in foreground {
        let pid = process["pid"]
            .as_u64()
            .ok_or_else(|| FlowError::guard("Herdr foreground PID missing"))?;
        ensure(
            pid == shell
                || crate::task_env::process_has_marker(pid as i32, &required(binding, "instance")?),
            "Herdr pane contains an unrelated foreground process; refusing close",
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn replaced_terminal_or_session_cannot_be_closed() {
        let binding = json!({"terminal":"original-terminal","session":"owned"});
        let mut pane = json!({"terminal_id":"original-terminal","agent_session":{"value":"owned"}});
        assert!(verify_pane(&binding, &pane).is_ok());
        pane["terminal_id"] = json!("replacement-terminal");
        assert!(verify_pane(&binding, &pane).is_err());
        pane["terminal_id"] = binding["terminal"].clone();
        pane["agent_session"]["value"] = json!("user-session");
        assert!(verify_pane(&binding, &pane).is_err());
    }
    #[test]
    fn unrelated_foreground_is_not_owned_by_pane() {
        let binding = json!({"instance":"unrelated-test-instance"});
        let pid = std::process::id();
        let mut processes = json!({"shell_pid":pid,"foreground_processes":[{"pid":pid}]});
        assert!(safe_foreground(&binding, &processes).is_ok());
        processes["shell_pid"] = json!(0);
        assert!(safe_foreground(&binding, &processes).is_err());
    }
}

#[cfg(test)]
#[path = "../../../tests/common/native_frontend_pty.rs"]
mod pty_tests;
