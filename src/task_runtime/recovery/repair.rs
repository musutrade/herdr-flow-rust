//! One model-requested repair of the existing read-only receiver watcher.
//! An interrupted spawn is observed by its recorded identity, never repeated.
use super::*;
use crate::queue;
use crate::task_runtime::native::receiver::{active, recovery};

pub(super) fn advance(root: &Path, context: &J, incident: &mut J) -> Result<()> {
    let result = if incident["repair"].is_null() {
        attempt(root, context, incident)
    } else {
        reconcile(context, incident).map(|()| true)
    };
    if matches!(result, Ok(false)) {
        incident["repair_wait"] = json!({"reason":"control checkpoint or watcher lock is busy",
            "next_step":"Wait for the existing lock/checkpoint to finish, then revalidate the original operation before any spawn.",
            "operation_replay_allowed":false});
        return save(root, "incident.json", incident);
    }
    incident.as_object_mut().unwrap().remove("repair_wait");
    if let Err(error) = result {
        let phase = if incident["repair"].is_null() {
            "blocked"
        } else {
            "unknown"
        };
        incident["repair"]["phase"] = json!(phase);
        incident["repair"]["error"] = json!(error.to_string());
        incident["repair"]["next_step"] = json!("Inspect the recorded process and original operations; this incident will not spawn another watcher.");
    }
    // A second turn in the same recovery session verifies the actual outcome.
    // The earlier diagnosis/turn and the repair intent remain intact.
    incident["phase"] = json!("pending");
    save(root, "incident.json", incident)
}

struct Receiver {
    root: PathBuf,
    guard: PathBuf,
    state: J,
    runtime: J,
}

impl Receiver {
    fn load(context: &J, env: &TaskEnvironment) -> Result<Self> {
        let root = PathBuf::from(required(context, "coordinator")?);
        let runtime = verified_runtime(env, &root, context)?;
        let guard = receiver_guard(env, &root)?;
        let state = util::read_state_json(&guard, "state.json")?;
        registered_receiver(context, &state)?;
        Ok(Self {
            root,
            guard,
            state,
            runtime,
        })
    }

    fn original_operation(&self) -> Result<J> {
        ensure(
            util::read_state_json(&self.root, "receiver-start.json")?["phase"] == "submitted",
            "original coordinator send is unresolved",
        )?;
        recovery::no_unresolved_tools(&self.root)?;
        let turn = util::read_state_json(&self.root, "receiver-turn.json")?;
        ensure(
            turn["thread"] == self.state["session"],
            "original receiver turn changed",
        )?;
        let binding = json!({"session":turn["thread"],"turn":turn["turn"]});
        let actual = observe::upstream_turn(&self.runtime, &binding)?
            .ok_or_else(|| FlowError::guard("original coordinator turn is unobserved"))?;
        ensure(
            matches!(
                actual["status"].as_str(),
                Some("inProgress" | "completed" | "failed" | "interrupted")
            ),
            "original coordinator turn status is unknown",
        )?;
        Ok(json!({"binding":binding,"status":actual["status"],"operation_replay_allowed":false}))
    }
}

fn verified_runtime(env: &TaskEnvironment, root: &Path, context: &J) -> Result<J> {
    let runtime = super::super::verify(env, root)?;
    ensure(
        runtime == context["runtime"],
        "repair coordinator runtime changed",
    )?;
    Ok(runtime)
}

fn receiver_guard(env: &TaskEnvironment, root: &Path) -> Result<PathBuf> {
    let binding = util::read_state_json(root, "receiver.json")?;
    ensure(binding["phase"] == "bound", "repair receiver is not bound")?;
    let guard = PathBuf::from(required(&binding, "guard_dir")?);
    crate::task_env::validate_path(env, &guard)?;
    Ok(guard)
}

fn registered_receiver(context: &J, state: &J) -> Result<()> {
    let owner: Owner = serde_json::from_value(context["owner"].clone())?;
    let q = queue::load_queue(&owner.queue_root)?;
    ensure(
        q["stopped"] != true
            && q["receiver"] == crate::guard::scope(state)
            && state["phase"] == "monitoring"
            && state["instance"] == owner.instance_id,
        "repair receiver registration or control state changed",
    )?;
    active::Adapter::new(state)?;
    Ok(())
}

fn environment(context: &J) -> Result<(crate::lifecycle::Permit, TaskEnvironment)> {
    let owner: Owner = serde_json::from_value(context["owner"].clone())?;
    let permit = crate::lifecycle::write_permit(&owner)?;
    let env = crate::task_env::instance::allocation::restore(&owner, &permit)?;
    ensure(
        crate::task_env::isolation::owner(&env).as_ref() == Some(&owner),
        "repair environment owner changed",
    )?;
    Ok((permit, env))
}

fn attempt(root: &Path, context: &J, incident: &mut J) -> Result<bool> {
    let (permit, mut env) = environment(context)?;
    let receiver = Receiver::load(context, &env)?;
    let _runtime = crate::guard::locked_existing(&receiver.root)?;
    let _guard = crate::guard::locked_existing(&receiver.guard)?;
    let Some((_control, watch)) = available_locks(&receiver)? else {
        return Ok(false);
    };
    start_locked(root, &receiver, &mut env, &permit, incident, watch)?;
    Ok(true)
}

fn available_locks(receiver: &Receiver) -> Result<Option<(util::FileLock, util::FileLock)>> {
    let Some(control) =
        util::FileLock::acquire(&receiver.root.join("control-service.lock"), false)?
    else {
        return Ok(None);
    };
    if !super::super::broker::checkpoint::available(&receiver.root)? {
        return Ok(None);
    }
    let Some(watch) = util::FileLock::acquire(&receiver.guard.join("watch.lock"), false)? else {
        return Ok(None);
    };
    Ok(Some((control, watch)))
}

fn start_locked(
    root: &Path,
    receiver: &Receiver,
    env: &mut TaskEnvironment,
    permit: &crate::lifecycle::Permit,
    incident: &mut J,
    watch: util::FileLock,
) -> Result<()> {
    let previous = stopped_watcher(receiver, env, incident)?;
    let operation = receiver.original_operation()?;
    incident["repair"] = json!({"phase":"executing","kind":"restart_receiver_watch",
        "guard":receiver.guard,"previous":previous,"original_operation":operation,
        "operation_replay_allowed":false,"business_outcome":"unchanged"});
    save(root, "incident.json", incident)?;
    drop(watch); // The new child must acquire this same existing watcher lock.
    spawn(root, receiver, env, permit, incident)
}

fn stopped_watcher(receiver: &Receiver, env: &TaskEnvironment, incident: &J) -> Result<J> {
    let fault = &incident["fault"];
    ensure(
        fault["kind"] == "watcher_unavailable" && fault["source"] == json!(receiver.guard),
        "repair request is not for this receiver watcher",
    )?;
    let watch = util::read_state_json(&receiver.guard, "watch.json")?;
    ensure(
        watch["pid"] == fault["identity"]["pid"]
            && watch["start_ticks"] == fault["identity"]["start_ticks"],
        "observed watcher identity changed before repair",
    )?;
    let record = registered_process(env, &watch)?;
    ensure(
        !crate::task_env::observe_process_alive(&record)?,
        "recorded watcher is still alive; stale heartbeat does not authorize replacement",
    )?;
    Ok(watch)
}

fn registered_process(env: &TaskEnvironment, watch: &J) -> Result<crate::task_env::ProcessRecord> {
    let pid = watch["pid"]
        .as_i64()
        .filter(|p| *p > 1 && *p <= i32::MAX as i64)
        .ok_or_else(|| FlowError::guard("watcher pid missing"))? as i32;
    let ticks: u64 = required(watch, "start_ticks")?
        .parse()
        .map_err(|_| FlowError::guard("watcher start identity invalid"))?;
    ensure(
        env.child_processes
            .iter()
            .any(|p| p.pid == pid && p.start_time == Some(ticks)),
        "watcher is not a registered process of this task",
    )?;
    Ok(crate::task_env::ProcessRecord {
        pid,
        start_time: Some(ticks),
    })
}

fn spawn(
    root: &Path,
    receiver: &Receiver,
    env: &mut TaskEnvironment,
    permit: &crate::lifecycle::Permit,
    incident: &mut J,
) -> Result<()> {
    let mut child = active::spawn_watch(env, &receiver.guard, &receiver.runtime, Some(permit))?;
    let result = finish_spawn(root, receiver, env, &mut child, incident);
    // Registration precedes publication; even a lost readiness response leaves
    // the child owned by the original environment and its terminal cleanup.
    util::process::BackgroundChild {
        pid: child.id(),
        child,
        ready: None,
    }
    .reap();
    result
}

fn finish_spawn(
    root: &Path,
    receiver: &Receiver,
    env: &TaskEnvironment,
    child: &mut Child,
    incident: &mut J,
) -> Result<()> {
    let identity = util::process::process_identity(child.id())?;
    registered_process(env, &identity)?;
    incident["repair"]["watcher"] = identity;
    incident["repair"]["phase"] = json!("started");
    save(root, "incident.json", incident)?;
    active::await_watch(&receiver.guard, child)?;
    incident["repair"]["health"] = verified_watch(receiver, env, incident)?;
    publish_activation(receiver, incident)?;
    incident["repair"]["phase"] = json!("verified");
    save(root, "incident.json", incident)
}

fn verified_watch(receiver: &Receiver, env: &TaskEnvironment, incident: &J) -> Result<J> {
    ensure(
        incident["repair"]["guard"] == json!(receiver.guard),
        "repaired watcher location changed",
    )?;
    let expected = &incident["repair"]["watcher"];
    registered_process(env, expected)?;
    let health = crate::watch_service::health(&receiver.guard);
    ensure(
        health["healthy"] == true
            && health["pid"] == expected["pid"]
            && health["start_ticks"] == expected["start_ticks"],
        "repaired watcher has not confirmed its exact identity, lock and heartbeat",
    )?;
    active::observe(&receiver.guard)?;
    Ok(health)
}

fn reconcile(context: &J, incident: &mut J) -> Result<()> {
    ensure(
        matches!(
            incident["repair"]["phase"].as_str(),
            Some("started" | "verified")
        ),
        "repair spawn outcome is unknown; no replacement will be spawned",
    )?;
    let (_permit, env) = environment(context)?;
    let receiver = Receiver::load(context, &env)?;
    let _runtime = crate::guard::locked_existing(&receiver.root)?;
    let _guard = crate::guard::locked_existing(&receiver.guard)?;
    incident["repair"]["health"] = verified_watch(&receiver, &env, incident)?;
    publish_activation(&receiver, incident)?;
    incident["repair"]["phase"] = json!("verified");
    Ok(())
}

fn publish_activation(receiver: &Receiver, incident: &J) -> Result<()> {
    let mut activation = util::read_state_json(&receiver.guard, "activation.json")?;
    let expected = &incident["repair"]["watcher"]["pid"];
    ensure(
        activation["session"] == receiver.state["session"]
            && (activation["pid"] == incident["repair"]["previous"]["pid"]
                || activation["pid"] == *expected),
        "watcher activation identity changed during repair",
    )?;
    activation["pid"] = expected.clone();
    activation["recovered"] = json!(true);
    activation["recovery_incident"] = incident["id"].clone();
    util::atomic_write_json(&receiver.guard.join("activation.json"), &activation)
}

pub(super) fn verify_result(context: &J, incident: &J) -> Result<()> {
    ensure(
        incident["repair"]["phase"] == "verified",
        "unverified repair cannot be reported recovered",
    )?;
    let (_permit, env) = environment(context)?;
    let receiver = Receiver::load(context, &env)?;
    verified_watch(&receiver, &env, incident)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activation_publication_is_idempotent_but_never_overwrites_a_different_watcher() {
        let temp = util::fs::test_tempdir();
        let receiver = Receiver {
            root: temp.path().to_path_buf(),
            guard: temp.path().join("guard"),
            state: json!({"session":"same"}),
            runtime: json!({}),
        };
        let incident =
            json!({"id":"incident","repair":{"previous":{"pid":101},"watcher":{"pid":202}}});
        let mut activation =
            json!({"phase":"active","pid":101,"session":"same","recovery":{"phase":"watching"}});
        util::atomic_write_json(&receiver.guard.join("activation.json"), &activation).unwrap();
        publish_activation(&receiver, &incident).unwrap();
        let saved = util::read_state_json(&receiver.guard, "activation.json").unwrap();
        assert_eq!(saved["pid"], 202);
        assert_eq!(saved["recovery"], activation["recovery"]);
        publish_activation(&receiver, &incident).unwrap();
        assert_eq!(
            util::read_state_json(&receiver.guard, "activation.json").unwrap(),
            saved
        );
        for (key, value) in [("pid", json!(303)), ("session", json!("other"))] {
            activation[key] = value;
            util::atomic_write_json(&receiver.guard.join("activation.json"), &activation).unwrap();
            assert!(publish_activation(&receiver, &incident).is_err());
            assert_eq!(
                util::read_state_json(&receiver.guard, "activation.json").unwrap(),
                activation
            );
        }
    }

    #[test]
    fn interrupted_spawn_is_retained_without_a_second_spawn_or_changed_business_result() {
        let root = util::fs::test_tempdir();
        let mut incident = json!({"phase":"repairing","diagnosis":{"disposition":"repair_requested"},
            "repair":{"phase":"executing","kind":"restart_receiver_watch","original_operation":{"turn":"original","operation_replay_allowed":false}}});
        let original = incident["repair"]["original_operation"].clone();
        for _ in 0..2 {
            advance(root.path(), &json!({}), &mut incident).unwrap();
            assert_eq!(incident["repair"]["phase"], "unknown");
            assert_eq!(incident["phase"], "pending");
            assert_eq!(incident["repair"]["original_operation"], original);
            assert!(incident["repair"]["watcher"].is_null());
            assert!(incident["result"].is_null());
            assert!(verify_result(&json!({}), &incident).is_err());
        }
        assert_eq!(
            read(root.path(), "incident.json").unwrap().unwrap(),
            incident
        );
    }

    #[test]
    fn watcher_repair_requires_exact_registered_identity_and_positive_exit_observation() {
        let temp = util::fs::test_tempdir();
        let _registry = crate::task_env::test_env_root(&temp.path().join("registry"));
        let mut env = crate::task_env::create_environment(
            "repair-test",
            &temp.path().join("work"),
            vec![],
            &temp.path().join("tmp"),
        )
        .unwrap();
        let receiver = Receiver {
            root: temp.path().to_path_buf(),
            guard: temp.path().join("guard"),
            state: json!({}),
            runtime: json!({}),
        };
        let mut child = Command::new("sleep").arg("30").spawn().unwrap();
        let identity = util::process::process_identity(child.id()).unwrap();
        let mut watch = identity.clone();
        watch["heartbeat_at"] = json!(0);
        watch["status"] = json!("stopped");
        util::atomic_write_json(&receiver.guard.join("watch.json"), &watch).unwrap();
        let incident = json!({"fault":{"kind":"watcher_unavailable","source":receiver.guard,"identity":identity}});
        assert!(stopped_watcher(&receiver, &env, &incident).is_err());
        env.child_processes
            .push(crate::task_env::RegisteredProcess {
                pid: child.id() as i32,
                pgid: child.id() as i32,
                start_time: Some(identity["start_ticks"].as_str().unwrap().parse().unwrap()),
                descendants: vec![],
                direct_children: vec![],
            });
        assert!(stopped_watcher(&receiver, &env, &incident)
            .unwrap_err()
            .to_string()
            .contains("still alive"));
        child.kill().unwrap();
        child.wait().unwrap();
        assert_eq!(stopped_watcher(&receiver, &env, &incident).unwrap(), watch);
        for field in ["kind", "source", "identity"] {
            let mut changed = incident.clone();
            changed["fault"][field] = J::Null;
            assert!(stopped_watcher(&receiver, &env, &changed).is_err());
        }
        for identity in [
            json!({"pid":1}),
            json!({"pid":i64::MAX}),
            json!({"pid":child.id(),"start_ticks":"bad"}),
            json!({"pid":child.id(),"start_ticks":"0"}),
        ] {
            assert!(registered_process(&env, &identity).is_err());
        }
        for path in [
            receiver.root.join("control-service.lock"),
            receiver.guard.join("watch.lock"),
        ] {
            let held = util::FileLock::acquire(&path, false).unwrap().unwrap();
            assert!(available_locks(&receiver).unwrap().is_none());
            drop(held);
            assert!(available_locks(&receiver).unwrap().is_some());
        }
        env.child_processes.clear();
        crate::task_env::destroy_environment(&env).unwrap();
    }
}
