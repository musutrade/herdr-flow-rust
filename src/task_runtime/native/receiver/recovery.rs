//! Rebind an idle, exactly identified receiver after its transport restarted.
//! Never creates a thread, resends a turn, or reconciles unknown mutations by guess.
use super::*;

pub(crate) fn if_needed(queue_root: &Path) -> Result<()> {
    let Some((root, state, owner)) = candidate(queue_root)? else {
        return Ok(());
    };
    let permit = crate::lifecycle::runtime_permit(&root)?
        .ok_or_else(|| FlowError::guard("receiver owner missing"))?;
    ensure(
        permit.owner() == &owner,
        "recovery receiver belongs to another owner",
    )?;
    wait_for_old_watcher(&state)?;
    let mut env = crate::task_env::instance::allocation::restore(&owner, &permit)?;
    let runtime = util::read_state_json(&root, "runtime.json")?;
    recover(
        &mut env,
        &root,
        &json!({"driver_binary":runtime.get("driver_binary").cloned().unwrap_or(json!(std::env::current_exe()?))}),
    )?;
    Ok(())
}

fn candidate(queue_root: &Path) -> Result<Option<(PathBuf, J, crate::lifecycle::Owner)>> {
    let _lock = queue::locked(queue_root, false)?;
    let q = queue::load_queue(queue_root)?;
    if q["receiver"]["codex_runtime"].is_null() || q["stopped"] == true {
        return Ok(None);
    }
    let task = q["receiver"]["task"].as_str().unwrap_or_default();
    if !q["tasks"][task]["cleanup_obligation"].is_null() {
        return Ok(None);
    }
    registered_candidate(&q, task)
}

fn registered_candidate(
    q: &J,
    task: &str,
) -> Result<Option<(PathBuf, J, crate::lifecycle::Owner)>> {
    let guard = PathBuf::from(required(q, "guard_dir")?);
    let state = util::read_state_json(&guard, "state.json")?;
    let root = PathBuf::from(required(&state, "runtime")?);
    let runtime = util::read_state_json(&root, "runtime.json")?;
    if !needs_rebind(&root, &runtime, &state)? {
        return Ok(None);
    }
    let owner = receiver_owner(q, task, &state)?;
    Ok(Some((root, state, owner)))
}

fn receiver_owner(q: &J, task: &str, state: &J) -> Result<crate::lifecycle::Owner> {
    ensure(
        q["receiver"] == crate::guard::scope(state),
        "recovery receiver registration changed",
    )?;
    crate::lifecycle::task_owner(q, task)?.ok_or_else(|| FlowError::guard("receiver owner missing"))
}

fn needs_rebind(root: &Path, runtime: &J, state: &J) -> Result<bool> {
    Ok(!same_process(runtime, state)
        || !util::read_state_json(root, "receiver.json")?["rebind"].is_null())
}

fn same_process(runtime: &J, state: &J) -> bool {
    runtime["pid"] == state["runtime_pid"] && runtime["start_time"] == state["runtime_start_time"]
}

fn wait_for_old_watcher(state: &J) -> Result<()> {
    let guard = PathBuf::from(required(state, "state_dir")?);
    if crate::watch_service::health(&guard)["healthy"] == true {
        return Err(FlowError::new(
            crate::ErrorKind::StateBusy,
            "waiting for old receiver watcher to stop",
        ));
    }
    Ok(())
}

struct Recovery {
    runtime: J,
    journal: J,
    guard: PathBuf,
    state: J,
}

impl Recovery {
    fn load(env: &TaskEnvironment, root: &Path) -> Result<Self> {
        let runtime = super::started_runtime(env, root)?;
        ensure(
            super::super::super::upstream::selected(&runtime),
            "receiver recovery requires upstream attachment",
        )?;
        let journal = util::read_state_json(root, "receiver.json")?;
        ensure(
            journal["phase"] == "bound",
            "receiver creation is unresolved",
        )?;
        let guard = PathBuf::from(required(&journal, "guard_dir")?);
        crate::task_env::validate_path(env, &guard)?;
        let state = util::read_state_json(&guard, "state.json")?;
        Ok(Self {
            runtime,
            journal,
            guard,
            state,
        })
    }

    fn already_active(&self) -> bool {
        same_process(&self.runtime, &self.state)
            && crate::watch_service::health(&self.guard)["healthy"] == true
    }

    fn finish_active(&mut self, root: &Path) -> Result<J> {
        active::Adapter::new(&self.state)?;
        if !self.journal["rebind"].is_null() {
            ensure(
                self.journal["rebind"]["runtime"] == self.runtime
                    && self.journal["rebind"]["to"] == self.state,
                "recovery publication identity changed",
            )?;
        }
        self.complete(root)?;
        util::read_state_json(&self.guard, "activation.json")
    }

    fn publish(&mut self, root: &Path) -> Result<()> {
        let planned = plan(root, &self.runtime, &self.journal, &self.state)?;
        ensure_idle(root, &planned)?;
        self.reserve(root, &planned)?;
        util::atomic_write_json(&self.guard.join("state.json"), &planned)?;
        self.state = planned;
        self.journal["state"] = self.state.clone();
        self.journal["runtime"] = self.runtime.clone();
        util::atomic_write_json(&root.join("receiver.json"), &self.journal)
    }

    fn reserve(&mut self, root: &Path, planned: &J) -> Result<()> {
        if self.journal["rebind"].is_null() {
            self.journal["rebind"] = json!({"from":self.state,"to":planned,"runtime":self.runtime});
            util::atomic_write_json(&root.join("receiver.json"), &self.journal)?;
        }
        Ok(())
    }

    fn complete(&mut self, root: &Path) -> Result<()> {
        self.journal.as_object_mut().unwrap().remove("rebind");
        util::atomic_write_json(&root.join("receiver.json"), &self.journal)
    }

    fn restart_watch(
        &mut self,
        env: &mut TaskEnvironment,
        root: &Path,
        spec: &J,
        permit: Option<&crate::lifecycle::Permit>,
    ) -> Result<J> {
        let mut child = active::spawn_watch(env, &self.guard, spec, permit)?;
        active::await_watch(&self.guard, &mut child)?;
        let result = json!({"phase":"active","pid":child.id(),"session":self.state["session"],
            "safe_rotation":false,"context_telemetry":false,"recovered":true});
        util::atomic_write_json(&self.guard.join("activation.json"), &result)?;
        util::process::BackgroundChild {
            pid: child.id(),
            child,
            ready: None,
        }
        .reap();
        self.complete(root)?;
        Ok(result)
    }
}

pub fn recover(env: &mut TaskEnvironment, root: &Path, spec: &J) -> Result<J> {
    let permit = crate::task_env::isolation::mutation_permit(env)?;
    let _runtime_lock = crate::guard::locked_existing(root)?;
    let mut recovery = Recovery::load(env, root)?;
    let guard_lock = crate::guard::locked_existing(&recovery.guard)?;
    if recovery.already_active() {
        return recovery.finish_active(root);
    }
    recovery.publish(root)?;
    drop(guard_lock);
    recovery.restart_watch(env, root, spec, permit.as_deref())
}

fn plan(root: &Path, runtime: &J, journal: &J, state: &J) -> Result<J> {
    no_unresolved_tools(root)?;
    if !journal["rebind"].is_null() {
        return saved_plan(runtime, &journal["rebind"], state);
    }
    receiver_identity(journal, state)?;
    receiver_configuration(journal, state)?;
    unchanged_runtime(runtime, journal)?;
    absent_broker(state)?;
    let guard = PathBuf::from(required(state, "state_dir")?);
    ensure(
        crate::watch_service::health(&guard)["healthy"] != true,
        "old receiver watcher still active",
    )?;
    let mut next = state.clone();
    next["updated_at"] = json!(util::now());
    next["runtime_pid"] = runtime["pid"].clone();
    next["runtime_start_time"] = runtime["start_time"].clone();
    Ok(next)
}

fn saved_plan(runtime: &J, saved: &J, state: &J) -> Result<J> {
    ensure(
        saved["runtime"] == *runtime && (*state == saved["from"] || *state == saved["to"]),
        "recovery publication identity changed",
    )?;
    Ok(saved["to"].clone())
}

fn receiver_identity(journal: &J, state: &J) -> Result<()> {
    ensure(
        is_task_owned(state)
            && state["receiver_activation"] == "active"
            && state["session"] == journal["state"]["session"],
        "receiver binding changed",
    )?;
    Ok(())
}

fn receiver_configuration(journal: &J, state: &J) -> Result<()> {
    let mut registered = journal["state"].clone();
    for key in ["updated_at", "phase", "receiver_activation"] {
        if let Some(value) = state.get(key) {
            registered[key] = value.clone();
        } else if let Some(fields) = registered.as_object_mut() {
            fields.remove(key);
        }
    }
    ensure(
        registered == *state && state["phase"] == "monitoring",
        "receiver configuration changed",
    )?;
    Ok(())
}

fn unchanged_runtime(runtime: &J, journal: &J) -> Result<()> {
    let mut previous = journal["runtime"].clone();
    previous["pid"] = runtime["pid"].clone();
    previous["start_time"] = runtime["start_time"].clone();
    ensure(
        previous == *runtime,
        "recovery changed more than broker process identity",
    )?;
    Ok(())
}

fn absent_broker(state: &J) -> Result<()> {
    let pid = state["runtime_pid"]
        .as_u64()
        .filter(|p| *p > 1 && *p <= i32::MAX as u64)
        .ok_or_else(|| FlowError::guard("old broker pid missing"))?;
    // SAFETY: signal zero only queries kernel existence; no process is signalled.
    let absent = unsafe { libc::kill(pid as i32, 0) } == -1
        && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
    ensure(absent, "old broker is not confirmed absent")?;
    Ok(())
}

fn no_unresolved_tools(root: &Path) -> Result<()> {
    for (file, complete) in [
        ("broker-tools.json", "responded"),
        ("control-tools.json", "complete"),
    ] {
        let journal =
            util::read_state_json_bounded_if_exists(root, file, 1024 * 1024)?.unwrap_or(json!({}));
        let entries = journal
            .as_object()
            .ok_or_else(|| FlowError::guard("invalid tool journal"))?;
        ensure(
            entries.values().all(|e| e["phase"] == complete),
            "unresolved tool operation blocks receiver recovery",
        )?;
    }
    Ok(())
}

fn ensure_idle(root: &Path, state: &J) -> Result<()> {
    let response = client(root)?.call(
        "thread/read",
        &json!({"threadId":state["session"],"includeTurns":false}),
    )?;
    verify_thread(state, &response["thread"])?;
    ensure(
        response["thread"]["status"]["type"] == "idle"
            && response["thread"]["canAcceptDirectInput"] == true,
        "receiver must be loaded and idle; active or unknown turns require inspection",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rebind_requires_exact_configuration_dead_broker_and_resolved_tools() {
        let temp = util::fs::test_tempdir();
        let mut child = Command::new("/bin/true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        let runtime =
            json!({"pid":std::process::id(),"start_time":1,"model":"fixed","instance":"ours"});
        let state = json!({"adapter":"codex","receiver_kind":"task-runtime","receiver_activation":"active",
            "phase":"monitoring","runtime_pid":pid,"runtime_start_time":0,
            "state_dir":temp.path(),"session":"ours","model":"fixed"});
        let mut previous = runtime.clone();
        previous["pid"] = json!(pid);
        previous["start_time"] = json!(0);
        let journal = json!({"state":state,"runtime":previous});
        let next = plan(temp.path(), &runtime, &journal, &state).unwrap();
        assert_eq!(next["runtime_pid"], std::process::id());
        assert_eq!(next["session"], state["session"]);
        for key in ["model", "role_models", "instance", "socket"] {
            let mut other = runtime.clone();
            other[key] = json!("foreign");
            assert!(plan(temp.path(), &other, &journal, &state).is_err());
        }
        let mut alive = state.clone();
        alive["runtime_pid"] = json!(std::process::id());
        let mut matching = journal.clone();
        matching["state"] = alive.clone();
        assert!(plan(temp.path(), &runtime, &matching, &alive).is_err());
        let mut foreign = state.clone();
        foreign["model"] = json!("other");
        assert!(plan(temp.path(), &runtime, &journal, &foreign).is_err());
        for (file, phase) in [
            ("broker-tools.json", "sending"),
            ("control-tools.json", "executing"),
        ] {
            util::atomic_write_json(&temp.path().join(file), &json!({"request":{"phase":phase}}))
                .unwrap();
            assert!(plan(temp.path(), &runtime, &journal, &state).is_err());
            std::fs::remove_file(temp.path().join(file)).unwrap();
        }
        let mut interrupted = journal;
        interrupted["rebind"] = json!({"from":state,"to":next,"runtime":runtime});
        for image in [&state, &next] {
            assert_eq!(
                plan(temp.path(), &runtime, &interrupted, image).unwrap(),
                next
            );
        }
        assert!(plan(temp.path(), &runtime, &interrupted, &foreign).is_err());
    }
}
