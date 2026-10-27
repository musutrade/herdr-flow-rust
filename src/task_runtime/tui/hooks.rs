//! Replace only the installed Herdr SessionStart reporter for this task.
use super::*;

pub(super) fn discover(runtime: &J) -> Result<J> {
    let socket = upstream::endpoint(runtime)?;
    let result = crate::codex::rpc::Client::new(&socket.to_string_lossy())
        .call("hooks/list", &json!({"cwds":[runtime["cwd"]]}))?;
    select(&result, &runtime["cwd"])
}

fn select(result: &J, cwd: &J) -> Result<J> {
    let hooks = listed_hooks(result, cwd)?;
    let mut states = json!({});
    for hook in hooks {
        if hook["eventName"] != "sessionStart" || hook["handlerType"] != "command" {
            continue;
        }
        let command = hook["command"].as_str().unwrap_or_default();
        if !command.contains("herdr-agent-state.sh") {
            continue;
        }
        replaceable(hook, command)?;
        states[required(hook, "key")?] = json!({"enabled":false});
    }
    Ok(states)
}

fn listed_hooks<'a>(result: &'a J, cwd: &J) -> Result<&'a Vec<J>> {
    let rows = result["data"]
        .as_array()
        .ok_or_else(|| FlowError::guard("hook discovery response missing"))?;
    ensure(
        rows.len() == 1 && rows[0]["cwd"] == *cwd,
        "hook discovery cwd changed",
    )?;
    ensure(
        rows[0]["errors"].as_array().is_some_and(Vec::is_empty),
        "hook discovery failed",
    )?;
    rows[0]["hooks"]
        .as_array()
        .ok_or_else(|| FlowError::guard("hook list missing"))
}

fn replaceable(hook: &J, command: &str) -> Result<()> {
    // Do not disable composite/custom/security hooks that merely mention
    // the integration. Only Herdr's exact installed command is replaceable.
    let source = Path::new(hook["sourcePath"].as_str().unwrap_or_default());
    let script = source
        .parent()
        .unwrap_or(Path::new("/"))
        .join("herdr-agent-state.sh");
    let expected = format!("bash '{}' session", script.display());
    ensure(
        command == expected && hook["isManaged"] == false,
        "custom/managed Herdr hook requires explicit integration; refusing to disable it",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn replaces_only_exact_herdr_reporter_and_preserves_other_hooks() {
        let mut result = json!({"data":[{"cwd":"/task","errors":[],"hooks":[
            {"key":"herdr","eventName":"sessionStart","handlerType":"command","command":"bash '/user/herdr-agent-state.sh' session","sourcePath":"/user/hooks.json","isManaged":false},
            {"key":"security","eventName":"sessionStart","handlerType":"command","command":"check-security"}]}]});
        assert_eq!(
            select(&result, &json!("/task")).unwrap(),
            json!({"herdr":{"enabled":false}})
        );
        result["data"][0]["hooks"][0]["command"] =
            json!("bash '/user/herdr-agent-state.sh' session; check-security");
        assert!(select(&result, &json!("/task")).is_err());
        assert!(select(&result, &json!("/foreign")).is_err());
    }
}
