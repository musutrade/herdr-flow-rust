//! Explicit activation for a fresh task receiver. Own polling process only;
//! existing guards/watchers and shared Codex services are never restarted.
use super::*;
use crate::herdr::ReceiverAdapter;

pub struct Adapter {
    state: J,
    root: PathBuf,
}
impl Adapter {
    pub fn new(state: &J) -> Result<Self> {
        ensure(
            is_task_owned(state) && state["receiver_activation"] == "active",
            "task receiver is not explicitly activated",
        )?;
        let root = PathBuf::from(required(state, "runtime")?);
        let adapter = Self {
            state: state.clone(),
            root,
        };
        adapter.verify()?;
        Ok(adapter)
    }
    fn verify(&self) -> Result<()> {
        let _permit = crate::lifecycle::runtime_permit(&self.root)?.ok_or_else(|| {
            FlowError::guard("task receiver requires isolated lifecycle authority")
        })?;
        let runtime = util::read_state_json(&self.root, "runtime.json")?;
        ensure(
            runtime["instance"] == self.state["instance"]
                && runtime["pid"] == self.state["runtime_pid"]
                && runtime["start_time"] == self.state["runtime_start_time"]
                && runtime["cwd"] == self.state["cwd"]
                && runtime["model"] == self.state["model"],
            "task receiver runtime binding changed",
        )?;
        let pid = runtime["pid"]
            .as_u64()
            .ok_or_else(|| FlowError::guard("runtime pid missing"))?;
        ensure(
            runtime["start_time"] == json!(crate::task_env::process_start_time(pid as i32))
                && !runtime["start_time"].is_null(),
            "task receiver broker is not alive",
        )?;
        let receiver = util::read_state_json(&self.root, "receiver.json")?;
        ensure(
            receiver["phase"] == "bound"
                && receiver["state"]["session"] == self.state["session"]
                && receiver["guard_dir"] == self.state["state_dir"],
            "task receiver identity changed",
        )
    }
}
impl ReceiverAdapter for Adapter {
    fn agent(&self) -> Result<J> {
        let _permit = crate::lifecycle::runtime_permit(&self.root)?;
        self.verify()?;
        let result = client(&self.root)?.call(
            "thread/read",
            &json!({"threadId":self.state["session"],"includeTurns":false}),
        )?;
        verify_thread(&self.state, &result["thread"])?;
        let status = &result["thread"]["status"]["type"];
        ensure(
            status == "idle" || status == "active",
            "task receiver thread is not loaded/healthy",
        )?;
        Ok(json!({"agent_session":{"value":self.state["session"]},
            "agent_status":if status == "idle" {"idle"} else {"working"},
            "interactive_ready":status == "idle" && result["thread"]["canAcceptDirectInput"] == true}))
    }
    fn prompt(&self, text: &str) -> Result<String> {
        let _permit = crate::lifecycle::runtime_permit(&self.root)?;
        ensure(
            self.agent()?["interactive_ready"] == true,
            "task receiver is busy",
        )?;
        let response = client(&self.root)?.call(
            "turn/start",
            &json!({"threadId":self.state["session"],
            "input":[{"type":"text","text":text}]}),
        )?;
        required(&response["turn"], "id")
    }
    fn call(&self, _args: &[String]) -> Result<J> {
        Err(FlowError::guard(
            "task receiver has no Herdr pane notification capability",
        ))
    }
}

pub fn activate(env: &mut TaskEnvironment, root: &Path, spec: &J) -> Result<J> {
    let permit = crate::task_env::isolation::mutation_permit(env)?;
    ensure(
        permit.is_some(),
        "receiver activation requires isolated task instance",
    )?;
    let runtime = super::started_runtime(env, root)?;
    let journal = util::read_state_json(root, "receiver.json")?;
    ensure(
        journal["phase"] == "bound",
        "receiver bootstrap is incomplete",
    )?;
    let guard = PathBuf::from(required(&journal, "guard_dir")?);
    crate::task_env::validate_path(env, &guard)?;
    let _lock = crate::guard::locked(&guard)?;
    ensure_absent(&guard.join("activation.json"))?;
    let mut state = util::read_state_json(&guard, "state.json")?;
    ensure(
        state["session"] == journal["state"]["session"]
            && state["instance"] == runtime["instance"]
            && state["phase"] == "bootstrap",
        "receiver activation binding changed",
    )?;
    util::atomic_write_json(
        &guard.join("activation.json"),
        &json!({"phase":"starting","session":state["session"]}),
    )?;
    state["receiver_activation"] = json!("active");
    state["phase"] = json!("monitoring");
    crate::guard::save(&guard, &mut state)?;
    let binary = model::string(spec, "driver_binary")
        .map(PathBuf::from)
        .unwrap_or(std::env::current_exe()?);
    let mut cmd = env.permitted_command(binary.as_os_str(), true, permit.as_deref())?;
    cmd.args(["task-runtime", "receiver-watch", "--state-dir"])
        .arg(&guard);
    let mut child = super::super::super::spawn_logged(&guard, &mut cmd)?;
    super::super::super::register_child(env, &mut child)?;
    drop(_lock);
    await_watch(&guard, &mut child)?;
    let result = json!({"phase":"active","pid":child.id(),"session":state["session"],
        "safe_rotation":false,"context_telemetry":false});
    util::atomic_write_json(&guard.join("activation.json"), &result)?;
    Ok(result)
}

fn await_watch(guard: &Path, child: &mut Child) -> Result<()> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        ensure(
            child.try_wait()?.is_none(),
            "task receiver watcher exited; inspect activation journal",
        )?;
        if crate::watch_service::health(guard)["healthy"] == true {
            return Ok(());
        }
        ensure(
            std::time::Instant::now() < deadline,
            "task receiver watcher readiness timed out",
        )?;
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

pub fn watch(guard: &Path) -> Result<()> {
    crate::watch_service::run(
        guard,
        || {
            let _permit = crate::lifecycle::runtime_permit(guard)?;
            let state = util::read_state_json(guard, "state.json")?;
            Adapter::new(&state)?.agent()?;
            Ok(false)
        },
        || {
            let state = util::read_state_json(guard, "state.json")?;
            ensure(
                std::env::var(crate::task_env::MARKER_ENV).ok().as_deref()
                    == state["instance"].as_str(),
                "task receiver watcher must belong to its task process tree",
            )?;
            Adapter::new(&state)?.agent().map(|_| ())
        },
        2,
        None,
        false,
    )
}
