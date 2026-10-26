//! Bounded coordinator tools executed by the existing queue driver.
//! Model commands never gain write access to the external control directory.
use super::*;
use crate::lifecycle::Owner;
mod continuation;
pub(crate) use continuation::drive;

pub(crate) fn definitions(state: &J) -> J {
    if !supported(state) {
        return json!([]);
    }
    if state["control_protocol"] == "queue-tool-v1" {
        return json!([{"name":"herdr_queue","description":"Inspect, ACK and apply the assigned result only. Stop after application.",
            "inputSchema":{"type":"object","properties":{"action":{"type":"string","enum":["inspect","ack","apply"]},"delivery":{"type":"string"},"spec":{"type":"string"}},"required":["action","delivery","spec"],"additionalProperties":false}}]);
    }
    json!([{"name":"herdr_queue","description":"Inspect your assigned task, acknowledge a notification delivery, or apply its recorded result. Continue only the assigned task using lineage (artifact/candidate/activity/continue), run (wait,instruction,evidence), deliver (existing authorized local Git target), or finalize (task,state_version,outcome,reason). No account, authorization creation, arbitrary shell or remote publication operations. run uses the operator-configured developer/reviewer model and this receiver upstream endpoint; no tool overrides or model fallback. It starts an existing linked activity; the queue driver collects its result. deliver requires adapter,target,ref,expected,authorization,state_version. All specs are JSON strings. For apply, spec is JSON with task, state_version (not expected_version), event, verdict and next_action. next_action requires kind, owner, reason, condition. A passed acceptance verdict requires kind delivery: this records a planned continuation and does not publish or grant authorization. Inspect supplies current task versions, events, immutable verdicts and runtime_results. For development apply include the collected output, acceptance evidence and release_evidence; acceptance reports cannot be rewritten. ACK confirms receipt only.",
        "inputSchema":{"type":"object","properties":{
            "action":{"type":"string","enum":["inspect","ack","apply","lineage","run","deliver","finalize"]},
            "delivery":{"type":"string","description":"Delivery ID for ack; empty otherwise"},
            "spec":{"type":"string","description":"JSON spec for apply, lineage, run, deliver or finalize; empty for inspect/ack"}},
            "required":["action","delivery","spec"],"additionalProperties":false}}])
}

pub(super) fn prompt(state: &J, text: &str) -> String {
    if !supported(state) {
        return text.to_owned();
    }
    if state["control_protocol"] == "queue-tool-v1" {
        return format!("{text}\nUse herdr_queue inspect, ack and apply only; stop after application. Unknown outcomes must not be replayed.");
    }
    format!("{text}\nFor this isolated receiver, replace the CLI/file instructions above with the herdr_queue tool: inspect; for a result notification ack the exact delivery, then apply its recorded result using the current version and immutable verdict ID. Never edit queue files or set session environment variables. Do not execute shell commands for control operations. For apply use spec JSON with keys task, state_version (not expected_version), event, verdict, next_action. next_action must contain kind, owner, reason, condition. For a passed acceptance verdict use kind delivery: this only plans a continuation and does not publish, deliver or grant permission. Continue the assigned task through existing lineage activities, run, independent review, authorized local delivery and finalize when applicable. A run reply confirms dispatch only; stop and wait for its result notification, do not poll. Never invent authorization or revise the requirement. Finalize success only after confirmed delivery; failures require an explicit final decision. On an unknown operation outcome stop and report the blocker.")
}

/// Caller holds the queue lock. Old receivers have no control-tool service.
pub(crate) fn service(queue_root: &Path) -> Result<()> {
    let q = queue::load_queue(queue_root)?;
    if q["receiver"]["codex_runtime"].is_null() || q["stopped"] == true {
        return Ok(());
    }
    let Some(guard) = q["guard_dir"].as_str() else {
        return Ok(());
    };
    let state = util::read_state_json(Path::new(guard), "state.json")?;
    if !supported(&state) {
        return Ok(());
    }
    serve_pending(queue_root, &q, &state)
}

fn serve_pending(queue_root: &Path, q: &J, state: &J) -> Result<()> {
    let root = PathBuf::from(required(state, "runtime")?);
    let _permit = bound_permit(queue_root, q, &root, state)?;
    let Some(requests) =
        util::read_state_json_bounded_if_exists(&root, "broker-tools.json", 1024 * 1024)?
    else {
        return Ok(());
    };
    let _service = util::FileLock::acquire(&root.join("control-service.lock"), false)?
        .ok_or_else(|| FlowError::new(crate::ErrorKind::StateBusy, "control service busy"))?;
    serve_bound(queue_root, q, &root, state, &requests)
}

fn serve_bound(queue_root: &Path, q: &J, root: &Path, state: &J, requests: &J) -> Result<()> {
    let permit = bound_permit(queue_root, q, root, state)?;
    let owner = permit.owner();
    let entries = requests
        .as_object()
        .ok_or_else(|| FlowError::guard("invalid tool requests"))?;
    for entry in entries.values().filter(|e| {
        e["phase"] == "pending"
            && (state["control_protocol"] != "queue-tool-v2"
                || !continuation::external(&e["request"]))
    }) {
        serve_request(root, state, owner, &entry["request"])?;
    }
    Ok(())
}

fn bound_permit(
    queue_root: &Path,
    q: &J,
    root: &Path,
    state: &J,
) -> Result<crate::lifecycle::Permit> {
    ensure(
        q["receiver"] == crate::guard::scope(state),
        "control receiver registration changed",
    )?;
    active::Adapter::new(state)?;
    let permit = crate::lifecycle::runtime_permit(root)?
        .ok_or_else(|| FlowError::guard("control requires isolated owner"))?;
    let owner = permit.owner();
    ensure(
        owner.queue_root == queue_root && q["id"] == owner.queue_id,
        "control queue changed",
    )?;
    crate::runtime::receiver_session(q, state["session"].as_str())?;
    Ok(permit)
}

fn serve_request(root: &Path, state: &J, owner: &Owner, request: &J) -> Result<()> {
    let mut journal =
        util::read_state_json_bounded_if_exists(root, "control-tools.json", 1024 * 1024)?
            .unwrap_or(json!({}));
    ensure(journal.is_object(), "invalid control journal")?;
    let key = request["id"].to_string();
    let result = if let Some(saved) = journal.get(&key) {
        ensure(saved["request"] == *request, "control request changed")?;
        ensure(
            saved["phase"] == "complete",
            "control operation outcome unknown; inspect without replay",
        )?;
        saved["result"].clone()
    } else {
        execute_once(root, state, owner, request, &mut journal, &key)?
    };
    super::super::super::broker::call(
        root,
        "tool/respond",
        &json!({"id":request["id"],"result":result}),
    )?;
    Ok(())
}

fn execute_once(
    root: &Path,
    state: &J,
    owner: &Owner,
    request: &J,
    journal: &mut J,
    key: &str,
) -> Result<J> {
    ensure(
        journal.as_object().unwrap().len() < 64,
        "control request bound reached",
    )?;
    journal[key] = json!({"phase":"executing","request":request});
    save(root, journal)?;
    let result = response(execute(root, state, owner, request));
    journal[key]["phase"] = json!("complete");
    journal[key]["result"] = result.clone();
    save(root, journal)?;
    Ok(result)
}

fn save(root: &Path, journal: &J) -> Result<()> {
    ensure(
        serde_json::to_vec(journal)?.len() <= 1024 * 1024,
        "control journal exceeds byte bound",
    )?;
    util::atomic_write_json(&root.join("control-tools.json"), journal)
}

fn response(result: Result<J>) -> J {
    match result {
        Ok(value) => {
            json!({"success":true,"contentItems":[{"type":"inputText","text":value.to_string()}]})
        }
        Err(error) => {
            json!({"success":false,"contentItems":[{"type":"inputText","text":error.to_string()}]})
        }
    }
}

fn execute(root: &Path, state: &J, owner: &Owner, request: &J) -> Result<J> {
    let args = validate_request(root, state, request)?;
    if continuation::external(request) {
        return continuation::execute(root, state, owner, args);
    }
    execute_queue(root, state, owner, args)
}

fn execute_queue(root: &Path, state: &J, owner: &Owner, args: &J) -> Result<J> {
    let q = queue::load_queue(&owner.queue_root)?;
    match args["action"].as_str() {
        Some("inspect") => inspect_runtime(root, &q, owner),
        Some("ack") => acknowledge(&q, state, owner, &required(args, "delivery")?),
        Some("apply") => apply(&q, state, owner, &parse_spec(args)?),
        Some("lineage") => continuation::lineage(&q, state, owner, &parse_spec(args)?),
        _ => Err(FlowError::guard("unsupported control action")),
    }
}

fn inspect_runtime(root: &Path, q: &J, owner: &Owner) -> Result<J> {
    let mut view = inspect(q, owner)?;
    view["runtime_results"] = continuation::results(root)?;
    Ok(view)
}

fn supported_action(state: &J, args: &J) -> bool {
    state["control_protocol"] == "queue-tool-v2"
        || matches!(args["action"].as_str(), Some("inspect" | "ack" | "apply"))
}

fn parse_spec(args: &J) -> Result<J> {
    Ok(serde_json::from_str(&required(args, "spec")?)?)
}

fn validate_request<'a>(root: &Path, state: &J, request: &'a J) -> Result<&'a J> {
    let params = &request["params"];
    let turn = util::read_state_json(root, "receiver-turn.json")?;
    ensure(
        params["threadId"] == state["session"]
            && params["turnId"] == turn["turn"]
            && turn["thread"] == state["session"],
        "foreign control thread/turn",
    )?;
    ensure(
        params["tool"] == "herdr_queue" && params["namespace"].is_null(),
        "unsupported control tool",
    )?;
    let args = &params["arguments"];
    validate_fields(args)?;
    ensure(
        supported_action(state, args),
        "continuations require a fresh queue-tool-v2 receiver",
    )?;
    Ok(args)
}

fn validate_fields(args: &J) -> Result<()> {
    let fields = args
        .as_object()
        .ok_or_else(|| FlowError::guard("invalid control arguments"))?;
    ensure(
        fields.len() == 3
            && ["action", "delivery", "spec"]
                .iter()
                .all(|k| args[k].is_string()),
        "unexpected control fields",
    )?;
    Ok(())
}

fn owns(q: &J, owner: &Owner, name: &str) -> bool {
    crate::lifecycle::task_owner(q, name)
        .ok()
        .flatten()
        .as_ref()
        == Some(owner)
}

fn inspect(q: &J, owner: &Owner) -> Result<J> {
    let tasks = q["tasks"]
        .as_object()
        .ok_or_else(|| FlowError::guard("task map missing"))?;
    let tasks: serde_json::Map<String, J> = tasks
        .iter()
        .filter(|(name, _)| owns(q, owner, name))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let events = q["events"]
        .as_object()
        .ok_or_else(|| FlowError::guard("event map missing"))?;
    let events: serde_json::Map<String, J> = events
        .iter()
        .filter(|(_, v)| tasks.contains_key(v["task"].as_str().unwrap_or_default()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    Ok(json!({"tasks":tasks,"events":events,"active_delivery":q["active"]}))
}

fn acknowledge(q: &J, state: &J, owner: &Owner, delivery: &str) -> Result<J> {
    let ids = q["deliveries"][delivery]["events"]
        .as_array()
        .ok_or_else(|| FlowError::guard("delivery events missing"))?;
    ensure(
        !ids.is_empty()
            && ids.iter().all(|id| {
                owns(
                    q,
                    owner,
                    q["events"][id.as_str().unwrap_or_default()]["task"]
                        .as_str()
                        .unwrap_or_default(),
                )
            }),
        "delivery contains another owner",
    )?;
    queue::acknowledge(&owner.queue_root, delivery, &required(state, "session")?)?;
    Ok(json!({"acknowledged":delivery}))
}

fn apply(q: &J, state: &J, owner: &Owner, spec: &J) -> Result<J> {
    let task = required(spec, "task")?;
    ensure(owns(q, owner, &task), "foreign task application")?;
    let fields = spec
        .as_object()
        .ok_or_else(|| FlowError::guard("invalid apply spec"))?;
    ensure(
        fields.keys().all(|k| {
            [
                "task",
                "event",
                "state_version",
                "verdict",
                "next_action",
                "output",
                "release_evidence",
                "archive_events",
                "acceptance",
                "execution_problem",
            ]
            .contains(&k.as_str())
        }),
        "unsupported apply fields",
    )?;
    ensure(
        spec["next_action"]["kind"] != "dispatch",
        "control tool cannot dispatch new work",
    )?;
    let updated = queue::task_action(
        &owner.queue_root,
        "task-apply",
        spec,
        state["session"].as_str(),
        &crate::herdr::receiver_factory,
    )?;
    Ok(json!({"applied":spec["event"],"task":updated["tasks"][task]}))
}

fn supported(state: &J) -> bool {
    matches!(
        state["control_protocol"].as_str(),
        Some("queue-tool-v1" | "queue-tool-v2")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn old_receivers_do_not_gain_continuation_tools() {
        let old = json!({"control_protocol":"queue-tool-v1","session":"ours"});
        let new = json!({"control_protocol":"queue-tool-v2","session":"ours"});
        assert_eq!(
            definitions(&old)[0]["inputSchema"]["properties"]["action"]["enum"],
            json!(["inspect", "ack", "apply"])
        );
        assert!(
            definitions(&new)[0]["inputSchema"]["properties"]["action"]["enum"]
                .as_array()
                .unwrap()
                .contains(&json!("run"))
        );
        assert_eq!(definitions(&json!({})), json!([]));
        assert!(prompt(&old, "notification").contains("stop after application"));
        assert_eq!(prompt(&json!({}), "notification"), "notification");
        let root = util::fs::test_tempdir();
        util::atomic_write_json(
            &root.path().join("receiver-turn.json"),
            &json!({"thread":"ours","turn":"current"}),
        )
        .unwrap();
        let request = json!({"params":{"threadId":"ours","turnId":"current","tool":"herdr_queue",
            "arguments":{"action":"run","delivery":"","spec":"{}"}}});
        assert!(validate_request(root.path(), &old, &request).is_err());
        assert!(validate_request(root.path(), &new, &request).is_ok());
    }
    fn fixture() -> (J, Owner) {
        let owner = Owner {
            queue_root: "/queue".into(),
            queue_id: "q".into(),
            task_id: "task".into(),
            epoch: 1,
            instance_id: "ours".into(),
        };
        let q = json!({"id":"q","lifecycle":{"queue_root":"/queue"},"tasks":{
            "task":{"epoch":1,"environment":{"instance_id":"ours"}},
            "review":{"lineage":{"requirement":"task"}},
            "foreign":{"epoch":1,"environment":{"instance_id":"theirs"}}},
            "events":{"a":{"task":"review"},"b":{"task":"foreign"}},"active":"delivery"});
        (q, owner)
    }
    #[test]
    fn inspection_and_application_are_owner_scoped() {
        let (q, owner) = fixture();
        let view = inspect(&q, &owner).unwrap();
        assert!(view["tasks"]["foreign"].is_null());
        assert!(view["events"]["b"].is_null());
        assert!(view["tasks"]["review"].is_object());
        assert!(view["events"]["a"].is_object());
        assert!(apply(&q, &json!({}), &owner, &json!({"task":"foreign"})).is_err());
        assert!(apply(
            &q,
            &json!({}),
            &owner,
            &json!({"task":"review","next_action":{"kind":"dispatch"}})
        )
        .is_err());
        assert!(apply(
            &q,
            &json!({}),
            &owner,
            &json!({"task":"review","authorization":"invented"})
        )
        .is_err());
        let mut mixed = q;
        mixed["deliveries"] = json!({"mixed":{"events":["a","b"]}});
        assert!(acknowledge(&mixed, &json!({}), &owner, "mixed").is_err());
    }
    #[test]
    fn interrupted_control_operation_is_never_reexecuted() {
        let root = util::fs::test_tempdir();
        let (_, owner) = fixture();
        let request = json!({"id":1,"method":"item/tool/call","params":{}});
        util::atomic_write_json(
            &root.path().join("control-tools.json"),
            &json!({"1":{"request":request,"phase":"executing"}}),
        )
        .unwrap();
        let before = util::read_state_json(root.path(), "control-tools.json").unwrap();
        assert!(serve_request(root.path(), &json!({}), &owner, &request).is_err());
        assert_eq!(
            util::read_state_json(root.path(), "control-tools.json").unwrap(),
            before
        );
        let saved = json!({"1":{"request":request,"phase":"complete","result":{"success":true,"contentItems":[]}}});
        util::atomic_write_json(&root.path().join("control-tools.json"), &saved).unwrap();
        // Returning a cached result cannot repeat the operation even when its
        // response transport is unavailable. Changed request identity is refused.
        assert!(serve_request(root.path(), &json!({}), &owner, &request).is_err());
        let mut changed = request;
        changed["params"]["different"] = json!(true);
        assert!(serve_request(root.path(), &json!({}), &owner, &changed).is_err());
        assert_eq!(
            util::read_state_json(root.path(), "control-tools.json").unwrap(),
            saved
        );
    }
    #[test]
    fn control_rejects_foreign_turn_and_unregistered_tools_before_queue_access() {
        let root = util::fs::test_tempdir();
        let (_, owner) = fixture();
        let state = json!({"session":"ours"});
        util::atomic_write_json(
            &root.path().join("receiver-turn.json"),
            &json!({"thread":"ours","turn":"current"}),
        )
        .unwrap();
        for params in [
            json!({"threadId":"foreign","turnId":"current","tool":"herdr_queue"}),
            json!({"threadId":"ours","turnId":"old","tool":"herdr_queue"}),
            json!({"threadId":"ours","turnId":"current","tool":"shell"}),
        ] {
            assert!(execute(root.path(), &state, &owner, &json!({"params":params})).is_err());
        }
    }
}
