//! Durable dynamic requests; no account or approval operations.
use super::*;

pub(super) fn record(root: &Path, event: &J) -> Result<()> {
    if event["method"] != "item/tool/call" {
        return Ok(());
    }
    ensure(
        event["id"].is_number() || event["id"].is_string(),
        "tool request id missing",
    )?;
    let mut journal = load(root)?;
    let key = event["id"].to_string();
    if let Some(old) = journal.get(&key) {
        return ensure(old["request"] == *event, "tool request id reused");
    }
    journal[&key] = json!({"request":event,"phase":"pending"});
    save(root, &journal)
}

fn save(root: &Path, journal: &J) -> Result<()> {
    super::capacity::tools(root, "broker-tools.json", journal)?;
    util::atomic_write_json(&root.join("broker-tools.json"), journal)
}

fn load(root: &Path) -> Result<J> {
    let value = util::read_state_json_bounded_if_exists(root, "broker-tools.json", 1024 * 1024)?
        .unwrap_or(json!({}));
    ensure(value.is_object(), "invalid tool journal")?;
    Ok(value)
}

pub(super) fn respond(
    root: &Path,
    connection: &mut Option<super::super::native::DirectClient>,
    params: &J,
) -> Result<J> {
    let mut journal = load(root)?;
    let key = params["id"].to_string();
    validate_control(root, &journal, &key, params)?;
    let entry = &mut journal[&key];
    ensure(
        entry["request"]["id"] == params["id"] && !params["id"].is_null(),
        "unknown tool response",
    )?;
    if entry["phase"] == "responded" {
        ensure(entry["result"] == params["result"], "tool response changed")?;
        return Ok(json!({"responded":true}));
    }
    send_pending(root, connection, params, journal, &key)
}

fn send_pending(
    root: &Path,
    connection: &mut Option<super::super::native::DirectClient>,
    params: &J,
    mut journal: J,
    key: &str,
) -> Result<J> {
    let entry = &mut journal[key];
    validate_pending(entry, params)?;
    let client = connection
        .as_mut()
        .ok_or_else(|| FlowError::guard("original tool connection unavailable"))?;
    entry["phase"] = json!("sending");
    entry["result"] = params["result"].clone();
    save(root, &journal)?;
    client.respond_tool(&params["id"], &params["result"])?;
    journal[key]["phase"] = json!("responded");
    save(root, &journal)?;
    Ok(json!({"responded":true}))
}

fn validate_control(root: &Path, journal: &J, key: &str, params: &J) -> Result<()> {
    let control = util::read_state_json(root, "control-tools.json")?;
    ensure(
        control[key]["phase"] == "complete"
            && control[key]["request"] == journal[key]["request"]
            && control[key]["result"] == params["result"],
        "tool response is not the recorded control result",
    )?;
    Ok(())
}
fn validate_pending(entry: &J, params: &J) -> Result<()> {
    ensure(
        entry["phase"] == "pending",
        "tool response outcome unknown; do not replay",
    )?;
    ensure(
        params["result"]["success"].is_boolean() && params["result"]["contentItems"].is_array(),
        "invalid tool result",
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dynamic_request_identity_is_immutable_and_approval_is_not_a_tool() {
        let root = util::fs::test_tempdir();
        record(
            root.path(),
            &json!({"id":1,"method":"item/commandExecution/requestApproval"}),
        )
        .unwrap();
        assert!(!root.path().join("broker-tools.json").exists());
        let request = json!({"id":1,"method":"item/tool/call","params":{"tool":"herdr_queue"}});
        record(root.path(), &request).unwrap();
        let before = load(root.path()).unwrap();
        record(root.path(), &request).unwrap();
        assert_eq!(load(root.path()).unwrap(), before);
        let mut changed = request;
        changed["params"]["tool"] = json!("other");
        assert!(record(root.path(), &changed).is_err());
        assert_eq!(load(root.path()).unwrap(), before);
    }
    #[test]
    fn completed_response_is_idempotent_but_interrupted_send_is_not_replayed() {
        let root = util::fs::test_tempdir();
        let request = json!({"id":1,"method":"item/tool/call","params":{"tool":"herdr_queue"}});
        let result = json!({"success":true,"contentItems":[]});
        let params = json!({"id":1,"result":result});
        util::atomic_write_json(
            &root.path().join("control-tools.json"),
            &json!({"1":{"phase":"complete","request":request,"result":result}}),
        )
        .unwrap();
        for phase in ["responded", "sending"] {
            let journal = json!({"1":{"phase":phase,"request":request,"result":result}});
            util::atomic_write_json(&root.path().join("broker-tools.json"), &journal).unwrap();
            assert_eq!(
                respond(root.path(), &mut None, &params).is_ok(),
                phase == "responded"
            );
            assert_eq!(load(root.path()).unwrap(), journal);
        }
        assert!(respond(
            root.path(),
            &mut None,
            &json!({"id":1,"result":{"success":false,"contentItems":[]}})
        )
        .is_err());
    }

    #[test]
    fn response_without_recorded_control_result_is_refused() {
        let root = util::fs::test_tempdir();
        assert!(respond(
            root.path(),
            &mut None,
            &json!({"id":1,"result":{"success":true,"contentItems":[]}})
        )
        .is_err());
    }
}
