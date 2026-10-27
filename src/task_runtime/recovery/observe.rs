//! Read actual journals/processes and the existing service, without depending
//! on the receiver watcher/broker or acquiring the queue transaction lock.
use super::*;

pub(super) fn scan(context: &J, q: &J) -> Result<Option<J>> {
    let coordinator = PathBuf::from(required(context, "coordinator")?);
    if read(&coordinator, "receiver-start.json")?.is_none() {
        return Ok(None);
    }
    if let Some(fault) = watchers(context)? {
        return Ok(Some(fault));
    }
    scan_runtimes(context, q)
}

fn scan_runtimes(context: &J, q: &J) -> Result<Option<J>> {
    let parent = PathBuf::from(required(context, "runtime_parent")?);
    for entry in std::fs::read_dir(&parent)? {
        let root = entry?.path();
        if let Some(fault) = registered_fault(&root, context, q)? {
            return Ok(Some(fault));
        }
    }
    Ok(None)
}

fn registered_fault(root: &Path, context: &J, q: &J) -> Result<Option<J>> {
    let Some(runtime) = read(root, "runtime.json")? else {
        return Ok(None);
    };
    if runtime["recovery_role"] == true || runtime["lifecycle_owner"] != context["owner"] {
        return Ok(None);
    }
    ensure(
        runtime["root"] == json!(root) && util::resolve_path(root) == root,
        "observed runtime path changed",
    )?;
    runtime_fault(root, &runtime, q)
}

fn watchers(context: &J) -> Result<Option<J>> {
    let coordinator = PathBuf::from(required(context, "coordinator")?);
    let receiver = util::read_state_json(&coordinator, "receiver.json")?;
    let guard = PathBuf::from(required(&receiver, "guard_dir")?);
    let owner: Owner = serde_json::from_value(context["owner"].clone())?;
    for root in [guard, crate::queue::artifacts(&owner.queue_root)?] {
        // An explicitly stopped/missing watcher is an observation failure,
        // not evidence that its operation failed or that it is safe to resend.
        let health = crate::watch_service::health(&root);
        if health["healthy"] != true {
            let identity = read(&root, "watch.json")?.unwrap_or(J::Null);
            return Ok(Some(fault(
                &root,
                "watcher_unavailable",
                &json!({
                "pid":identity["pid"],"start_ticks":identity["start_ticks"],"heartbeat_at":identity["heartbeat_at"]}),
                health,
            )));
        }
    }
    Ok(None)
}

fn runtime_fault(root: &Path, runtime: &J, q: &J) -> Result<Option<J>> {
    let turn = selected_turn(root, q)?;
    let Some(turn) = turn else {
        return Ok(None);
    };
    let pid = runtime["pid"].as_u64().unwrap_or(0) as i32;
    let identity = json!({"pid":pid,"start_time":runtime["start_time"],
        "session":turn["session"],"turn":turn["turn"]});
    if runtime["start_time"].is_null()
        || runtime["start_time"] != json!(crate::task_env::process_start_time(pid))
    {
        return Ok(Some(fault(
            root,
            "broker_unavailable",
            &identity,
            json!({"reason":"The recorded transport process is no longer observed. The upstream task may still be running.","turn":turn}),
        )));
    }
    if !turn["observation_error"].is_null() {
        return Ok(Some(fault(
            root,
            "observation_unavailable",
            &identity,
            turn,
        )));
    }
    match upstream_turn(runtime, &turn) {
        Ok(Some(actual)) => turn_fault(root, &identity, actual),
        Ok(None) => Ok(Some(fault(root, "turn_unobserved", &identity, turn))),
        Err(error) => Ok(Some(fault(
            root,
            "observation_unavailable",
            &identity,
            json!({"kind":error.kind.type_name(),"message":error.to_string(),"turn":turn}),
        ))),
    }
}

fn selected_turn(root: &Path, q: &J) -> Result<Option<J>> {
    if let Some(turn) = read(root, "turn.json")? {
        let creation = util::read_state_json(root, "creation.json")?;
        let wait = &q["waits"][required(&creation, "wait")?];
        if wait.is_null() || wait["closed"] == true {
            return Ok(None);
        }
        return Ok(Some(turn));
    }
    let Some(turn) = read(root, "receiver-turn.json")? else {
        return Ok(None);
    };
    Ok(Some(json!({"session":turn["thread"],"turn":turn["turn"]})))
}

fn upstream_turn(runtime: &J, turn: &J) -> Result<Option<J>> {
    let socket = upstream::endpoint(runtime)?;
    let mut client = crate::codex::rpc::Client::new(&socket.to_string_lossy());
    client.timeout = 3.0;
    read_turn(runtime, turn, &mut |method, params| {
        client.call(method, params)
    })
}

fn read_turn(
    runtime: &J,
    turn: &J,
    call: &mut dyn FnMut(&str, &J) -> Result<J>,
) -> Result<Option<J>> {
    let session = required(turn, "session")?;
    required(turn, "turn")?;
    let thread = call(
        "thread/read",
        &json!({"threadId":session,"includeTurns":false}),
    )?;
    ensure(
        thread["thread"]["id"] == session && thread["thread"]["cwd"] == runtime["cwd"],
        "observed upstream thread changed",
    )?;
    let rows = read_turns(&session, call)?;
    find_turn(&rows, &turn["turn"])
}

fn read_turns(session: &str, call: &mut dyn FnMut(&str, &J) -> Result<J>) -> Result<J> {
    let reply = call(
        "thread/turns/list",
        &json!({"threadId":session,"limit":20,"itemsView":"full"}),
    );
    match reply {
        Ok(value) => Ok(value["data"].clone()),
        Err(error) if error.rpc_code == Some(-32601) => {
            let value = call(
                "thread/read",
                &json!({"threadId":session,"includeTurns":true}),
            )?;
            ensure(
                value["thread"]["id"] == session,
                "observed thread identity changed",
            )?;
            Ok(value["thread"]["turns"].clone())
        }
        Err(error) => Err(error),
    }
}

pub(super) fn find_turn(rows: &J, id: &J) -> Result<Option<J>> {
    let rows = rows
        .as_array()
        .ok_or_else(|| FlowError::guard("turn observation missing"))?;
    Ok(rows.iter().find(|row| row["id"] == *id).cloned())
}

fn turn_fault(root: &Path, identity: &J, turn: J) -> Result<Option<J>> {
    match turn["status"].as_str() {
        Some("inProgress" | "completed") => Ok(None),
        Some("failed" | "interrupted") => Ok(Some(fault(root, "turn_stopped", identity, turn))),
        _ => Ok(Some(fault(root, "turn_unobserved", identity, turn))),
    }
}

pub(super) fn fault(root: &Path, kind: &str, identity: &J, evidence: J) -> J {
    let key = json!({"source":root,"kind":kind,"identity":identity});
    let id = util::pyjson::sha256_hex(key.to_string().as_bytes());
    json!({"id":id,"source":root,"kind":kind,"identity":identity,"evidence":evidence,
        "operation_replay_allowed":false,"business_outcome":"unchanged"})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn long_running_turns_are_healthy_and_unrecognized_or_stopped_turns_need_observation() {
        for state in ["inProgress", "completed"] {
            assert!(turn_fault(
                Path::new("/task"),
                &json!({}),
                json!({"status":state,"created_at":0})
            )
            .unwrap()
            .is_none());
        }
        for (state, kind) in [
            ("failed", "turn_stopped"),
            ("interrupted", "turn_stopped"),
            ("futureStatus", "turn_unobserved"),
        ] {
            assert_eq!(
                turn_fault(Path::new("/task"), &json!({}), json!({"status":state}))
                    .unwrap()
                    .unwrap()["kind"],
                kind
            );
        }
    }
    #[test]
    fn independent_reads_verify_thread_and_only_return_the_original_turn() {
        let runtime = json!({"cwd":"/task"});
        let selected = json!({"session":"s","turn":"original"});
        let rows =
            json!([{"id":"another","status":"completed"},{"id":"original","status":"inProgress"}]);
        for fallback in [false, true] {
            let result = read_turn(&runtime, &selected, &mut |method, params| {
                assert_eq!(params["threadId"], "s");
                if method == "thread/turns/list" {
                    if fallback {
                        let mut error = FlowError::rpc("unsupported");
                        error.rpc_code = Some(-32601);
                        return Err(error);
                    }
                    return Ok(json!({"data":rows}));
                }
                assert_eq!(method, "thread/read");
                Ok(json!({"thread":{"id":"s","cwd":"/task","turns":rows}}))
            })
            .unwrap()
            .unwrap();
            assert_eq!(result["id"], "original");
            assert_eq!(result["status"], "inProgress");
        }
        assert!(read_turn(&runtime, &selected, &mut |_, _| Ok(
            json!({"thread":{"id":"foreign","cwd":"/task"}})
        ))
        .is_err());
        assert!(read_turn(&runtime, &selected, &mut |_, _| Err(
            FlowError::rpc_timeout("read unavailable")
        ))
        .is_err());
    }
    #[test]
    fn dead_broker_is_not_a_failed_business_task_and_closed_activity_is_ignored() {
        let dir = crate::util::fs::test_tempdir();
        let root = dir.path();
        save(
            root,
            "turn.json",
            &json!({"turn":"original","session":"s","phase":"submitted"}),
        )
        .unwrap();
        save(root, "creation.json", &json!({"wait":"w"})).unwrap();
        let q = json!({"waits":{"w":{"closed":false}}});
        let fault = runtime_fault(root, &json!({"pid":0}), &q).unwrap().unwrap();
        assert_eq!(fault["kind"], "broker_unavailable");
        assert_eq!(fault["business_outcome"], "unchanged");
        assert!(runtime_fault(
            root,
            &json!({"pid":0}),
            &json!({"waits":{"w":{"closed":true}}})
        )
        .unwrap()
        .is_none());
        let live = json!({"pid":std::process::id(),"start_time":crate::task_env::process_start_time(std::process::id() as i32)});
        assert_eq!(
            runtime_fault(root, &live, &q).unwrap().unwrap()["kind"],
            "observation_unavailable"
        );
        save(
            root,
            "turn.json",
            &json!({"turn":"original","session":"s","observation_error":{"message":"cannot read"}}),
        )
        .unwrap();
        assert_eq!(
            runtime_fault(root, &live, &q).unwrap().unwrap()["kind"],
            "observation_unavailable"
        );
    }

    #[test]
    fn scan_ignores_foreign_and_recovery_sessions_but_observes_the_owned_coordinator() {
        let dir = crate::util::fs::test_tempdir();
        let owner = json!({"instance_id":"owned"});
        let context = json!({"runtime_parent":dir.path(),"owner":owner});
        for (name, record) in [
            (
                "foreign",
                json!({"lifecycle_owner":{"instance_id":"other"}}),
            ),
            (
                "diagnostic",
                json!({"lifecycle_owner":owner,"recovery_role":true}),
            ),
            ("coordinator", json!({"lifecycle_owner":owner,"pid":0})),
        ] {
            let root = dir.path().join(name);
            util::ensure_private_dir(&root, true).unwrap();
            let mut record = record;
            record["root"] = json!(root);
            save(&root, "runtime.json", &record).unwrap();
            save(
                &root,
                "receiver-turn.json",
                &json!({"thread":"s","turn":"t"}),
            )
            .unwrap();
        }
        let root = dir.path().join("coordinator");
        let fault = scan_runtimes(&context, &json!({})).unwrap().unwrap();
        assert_eq!(fault["source"], json!(root));
        assert_eq!(fault["kind"], "broker_unavailable");
        save(
            &root,
            "runtime.json",
            &json!({"lifecycle_owner":owner,"root":"/different"}),
        )
        .unwrap();
        assert!(scan_runtimes(&context, &json!({})).is_err());
        std::fs::remove_file(root.join("runtime.json")).unwrap();
        assert!(scan_runtimes(&context, &json!({})).unwrap().is_none());
    }
}
