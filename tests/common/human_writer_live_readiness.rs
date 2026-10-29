//! Read-only prerequisite for the one initial coordinator turn, not a controller.
use super::*;
type Check<T> = std::result::Result<T, String>;

pub(super) struct PreDispatch {
    expected: J,
    coordinator: PathBuf,
    queue: PathBuf,
    guard: PathBuf,
}
fn identity(pid: u32) -> J {
    json!({"pid":pid,"start_time":task_env::process_start_time(pid as i32).expect("watcher start identity unavailable")})
}
fn ticks(value: &J) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|s| s.parse().ok()))
}
fn require(condition: bool, reason: &str) -> Check<()> {
    condition.then_some(()).ok_or_else(|| reason.to_owned())
}
fn absent(path: &Path) -> bool {
    match fs::symlink_metadata(path) {
        Ok(_) => false,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
        Err(error) => panic!(
            "cannot inspect original dispatch marker {}: {error}",
            path.display()
        ),
    }
}
impl PreDispatch {
    pub fn new(
        f: &Fixture,
        coordinator: &Path,
        receiver_process: &J,
        watcher: &FixtureChild,
        configuration: &J,
    ) -> Self {
        let q = f.state();
        let receiver = util::read_state_json(coordinator, "receiver.json").unwrap();
        Self {
            coordinator: coordinator.into(),
            queue: f.queue.clone(),
            guard: PathBuf::from(receiver["guard_dir"].as_str().unwrap()),
            expected: json!({"owner":lifecycle::task_owner(&q,"task").unwrap().unwrap(),"receiver":q["receiver"],
                "receiver_process":receiver_process,"queue_process":identity(watcher.0.id()),
                "session_configuration":configuration}),
        }
    }
    pub fn observe(&self, watcher: &mut FixtureChild) -> J {
        let q = queue::load_queue(&self.queue).unwrap();
        let state = util::read_state_json(&self.guard, "state.json").unwrap();
        let receiver = util::read_state_json(&self.coordinator, "receiver.json").unwrap();
        let upstream = util::read_state_json(&self.coordinator, "upstream-session.json").unwrap();
        let mut configuration = json!({});
        for key in [
            "model",
            "modelProvider",
            "cwd",
            "reasoningEffort",
            "approvalPolicy",
            "approvalsReviewer",
            "sandbox",
        ] {
            configuration[key] = upstream["session_configuration"][key].clone();
        }
        // Same normalization as the production subscription comparison: cwd is
        // implicit. Additional writable roots and every other sandbox field stay exact.
        let cwd = configuration["cwd"].clone();
        if let Some(roots) = configuration["sandbox"]["writableRoots"].as_array_mut() {
            roots.retain(|root| *root != cwd);
        }
        json!({"at":util::now(),"owner":lifecycle::task_owner(&q,"task").unwrap(),"notify":q["notify"],"stopped":q["stopped"],
            "guard_dir":q["guard_dir"],"guard":self.guard,"receiver":q["receiver"],"guard_scope":guard::scope(&state),
            "guard_state":state,"registration":receiver,"queue_process":{"pid":watcher.0.id(),"start_time":task_env::process_start_time(watcher.0.id() as i32)},
            "queue_child_running":watcher.0.try_wait().unwrap().is_none(),
            "queue_health":herdr_flow::watch_service::health(&queue::artifacts(&self.queue).unwrap()),
            "receiver_health":herdr_flow::watch_service::health(&self.guard),
            "upstream_session":upstream["session"],"upstream_phase":upstream["phase"],"session_configuration":configuration,
            "original_start_absent":absent(&self.coordinator.join("receiver-start.json")) && absent(&self.coordinator.join("receiver-turn.json"))})
    }
    pub fn verify(&self, observation: &J) -> Check<()> {
        verify(observation, &self.expected)
    }
    pub fn dispatch<T>(&self, observation: J, send: impl FnOnce(J) -> T) -> Check<T> {
        self.verify(&observation)?;
        Ok(send(
            json!({"expected":self.expected,"observed":observation,"verified_before_first_turn":true}),
        ))
    }
}
fn verify(observation: &J, expected: &J) -> Check<()> {
    require(
        observation["owner"] == expected["owner"],
        "original Owner changed before dispatch",
    )?;
    require(
        observation["notify"] == true && observation["stopped"] == false,
        "queue notifications disabled or queue stopped",
    )?;
    require(
        observation["queue_process"] == expected["queue_process"],
        "queue child PID/start changed",
    )?;
    require(
        observation["queue_child_running"] == true,
        "original queue watcher exited",
    )?;
    for (kind, process) in [
        ("queue", &expected["queue_process"]),
        ("receiver", &expected["receiver_process"]),
    ] {
        let health = &observation[format!("{kind}_health")];
        require(
            health["healthy"] == true && health["status"] == "running",
            "watcher is not healthy/running",
        )?;
        require(
            health["pid"] == process["pid"]
                && ticks(&health["start_ticks"]) == process["start_time"].as_u64(),
            "watcher PID/start identity differs from original",
        )?;
        require(
            audit::exact_process_alive(process),
            "original watcher process exited",
        )?;
    }
    require(
        observation["guard_dir"] == observation["guard"]
            && observation["registration"]["guard_dir"] == observation["guard"],
        "guard directory binding changed",
    )?;
    require(
        observation["receiver"] == expected["receiver"]
            && observation["guard_scope"] == expected["receiver"],
        "queue/guard receiver scope changed",
    )?;
    require(
        observation["registration"]["phase"] == "bound"
            && observation["guard_state"]["receiver_activation"] == "active"
            && observation["guard_state"]["phase"] == "monitoring",
        "receiver is not activated/monitoring",
    )?;
    require(
        observation["guard_state"]["instance"] == expected["owner"]["instance_id"]
            && observation["registration"]["state"]["session"]
                == observation["guard_state"]["session"]
            && observation["upstream_phase"] == "created"
            && observation["upstream_session"] == observation["guard_state"]["session"],
        "original session/instance binding changed",
    )?;
    require(
        observation["original_start_absent"] == true,
        "original first send already has an outcome or intent; do not replay",
    )?;
    for key in ["approvalPolicy", "approvalsReviewer", "sandbox"] {
        require(
            !expected["session_configuration"][key].is_null()
                && observation["session_configuration"][key]
                    == expected["session_configuration"][key],
            "initial session permissions differ from explicit expectation",
        )?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "human_writer_live_readiness_tests.rs"]
mod tests;
