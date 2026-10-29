//! Real owned observer/brokers with a synthetic upstream; no user accounts.
use super::*;
#[path = "recovery_incident_server.rs"]
mod fixture_server;
use std::sync::Arc;
use std::time::{Duration, Instant};

const COORD: &str = "22222222-2222-2222-2222-222222222222";
const RECOVERY: &str = "44444444-4444-4444-4444-444444444444";

#[test]
fn stopped_watcher_gets_one_independent_recovery_result_and_all_resources_are_reclaimed() {
    exercise(Mode::Diagnose);
}

#[test]
fn recovery_model_failure_stays_visible_without_recursion_or_replay() {
    exercise(Mode::Failure);
}

#[test]
fn recovery_repairs_only_the_stopped_registered_watcher_then_verifies_and_hands_back() {
    exercise(Mode::Repair);
}

#[test]
fn recovery_repair_reports_environment_contention_without_restarting_or_replaying() {
    exercise(Mode::Busy);
}

#[test]
fn recovery_repair_keeps_unknown_control_operations_and_requests_human_verification() {
    exercise(Mode::Unknown);
}

#[test]
fn recovery_waits_for_control_checkpoint_before_its_only_spawn() {
    exercise(Mode::ControlBusy);
}

#[test]
fn recovery_waits_for_watcher_lock_before_its_only_spawn() {
    exercise(Mode::WatchBusy);
}

#[test]
fn interrupted_recovery_reconciles_the_recorded_watcher_without_spawning_another() {
    exercise(Mode::Reconcile);
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Diagnose,
    Failure,
    Repair,
    Busy,
    Unknown,
    Reconcile,
    ControlBusy,
    WatchBusy,
}

impl Mode {
    fn requests_repair(self) -> bool {
        matches!(
            self,
            Self::Repair
                | Self::Busy
                | Self::Unknown
                | Self::Reconcile
                | Self::ControlBusy
                | Self::WatchBusy
        )
    }
    fn succeeds(self) -> bool {
        matches!(
            self,
            Self::Repair | Self::Reconcile | Self::ControlBusy | Self::WatchBusy
        )
    }
}

fn exercise(mode: Mode) {
    let failed = mode == Mode::Failure;
    let f = Fixture::new();
    let mut env = isolated_runtime::linked(&f);
    let coordinator = task_runtime::prepare(&env, "coordinator").unwrap();
    let cwd = env.workspace_root.join("checkout");
    let requests = Arc::new(Mutex::new(Vec::<J>::new()));
    let recorded = requests.clone();
    let server_cwd = cwd.clone();
    let server = fixture_server::Server::start(
        if mode.succeeds() { 6 } else { 5 },
        Duration::from_millis(100),
        move |peer, connection| {
            serve(peer, connection, &server_cwd, &recorded, mode);
        },
    );
    let config = f.temp.path().join("user-config");
    fs::write(&config, "must remain unchanged").unwrap();
    let mut broker = task_runtime::launch(
        &mut env,
        &coordinator,
        &json!({
            "adapter":"codex","configuration":"upstream","socket":server.path,"cwd":cwd,
            "model":{"providerID":"existing","id":"coord","reasoning_effort":"high"},
            "role_models":{"developer":{"providerID":"existing","id":"dev"},
                "reviewer":{"providerID":"existing","id":"review"},
                "recovery":{"providerID":"existing","id":"diagnostic","reasoning_effort":"medium"}},
            "driver_binary":env!("CARGO_BIN_EXE_herdr-flow")
        }),
    )
    .unwrap();
    let guard = coordinator.join("guard");
    task_runtime::native::receiver::bootstrap(
        &env,
        &coordinator,
        &json!({"guard_dir":guard,"task":"task","role":"coordinator"}),
    )
    .unwrap();
    let activated = task_runtime::native::receiver::active::activate(
        &mut env,
        &coordinator,
        &json!({"driver_binary":env!("CARGO_BIN_EXE_herdr-flow")}),
    )
    .unwrap();
    assert_eq!(
        activated["recovery"]["phase"],
        "watching",
        "{activated}; {:?}",
        fs::read_to_string(coordinator.join("recovery-observer/server.log"))
    );
    let state = util::read_state_json(&guard, "state.json").unwrap();
    {
        let _lock = queue::locked(&f.queue, false).unwrap();
        let mut q = f.state();
        q["guard_dir"] = json!(guard);
        q["receiver"] = guard::scope(&state);
        queue::save(&f.queue, &mut q).unwrap();
    }
    task_runtime::native::receiver::start(
        &env,
        &coordinator,
        &json!({"instruction":"original task"}),
    )
    .unwrap();
    status_reader_does_not_stop_observer(&coordinator);
    let watcher = activated["pid"].as_u64().unwrap();
    assert!(env.owns_process(watcher as i32));
    assert!(Command::new("kill")
        .args(["-TERM", &watcher.to_string()])
        .status()
        .unwrap()
        .success());
    let workspace = env.workspace_root.clone();
    let scratch = env.temp_root.clone();
    let original = util::read_state_json(&coordinator, "receiver-start.json").unwrap();
    let registration = util::read_state_json(&coordinator, "receiver.json").unwrap();
    let waiting_lock = hold_repair_lock(&coordinator, &guard, mode);
    let unknown = json!({"uncertain":{"phase":"executing","request":{"id":"uncertain"}}});
    if mode == Mode::Unknown {
        util::atomic_write_json(&coordinator.join("control-tools.json"), &unknown).unwrap();
    }
    let env = if mode.succeeds() || mode == Mode::Unknown {
        drop(env);
        None
    } else {
        Some(env)
    };
    if let Some(lock) = waiting_lock {
        let deadline = Instant::now() + Duration::from_secs(20);
        let waiting = loop {
            let status = task_runtime::recovery::status(&coordinator).unwrap();
            if !status["incident"]["repair_wait"].is_null() {
                break status["incident"].clone();
            }
            assert!(
                Instant::now() < deadline,
                "{status}; fixture: {}",
                fixture_evidence(&coordinator, &mut broker, &server)
            );
            std::thread::sleep(Duration::from_millis(25));
        };
        std::thread::sleep(Duration::from_secs(3));
        let current = read_incident(&coordinator);
        assert_eq!(current["id"], waiting["id"]);
        assert_eq!(current["phase"], "repairing");
        assert!(current["repair"].is_null());
        assert_eq!(
            requests
                .lock()
                .unwrap()
                .iter()
                .filter(|r| r["method"] == "turn/start" && r["params"]["threadId"] == RECOVERY)
                .count(),
            1
        );
        assert_eq!(
            util::read_state_json(&guard, "activation.json").unwrap()["pid"],
            watcher
        );
        drop(lock);
    }
    let checkpoint_pid =
        (mode == Mode::Reconcile).then(|| restart_from_spawn_checkpoint(&coordinator));
    // Deliberately retain the environment lock until the diagnostic result:
    // a stalled original operation must not prevent the independent role.
    let deadline = Instant::now() + Duration::from_secs(30);
    let incident = loop {
        let status = task_runtime::recovery::status(&coordinator).unwrap();
        if status["incident"]["phase"] == if failed { "unavailable" } else { "handled" } {
            break status["incident"].clone();
        }
        assert!(
            Instant::now() < deadline,
            "{status}; fixture: {}",
            fixture_evidence(&coordinator, &mut broker, &server)
        );
        std::thread::sleep(Duration::from_millis(25));
    };
    if failed {
        assert!(incident["result"].is_null());
        assert!(incident["next_step"]
            .as_str()
            .unwrap()
            .contains("before any retry"));
    } else {
        assert_eq!(
            incident["result"]["disposition"],
            if mode.succeeds() {
                "recovered"
            } else {
                "needs_human"
            },
            "{incident}"
        );
    }
    if mode.requests_repair() {
        assert_eq!(incident["diagnosis"]["disposition"], "repair_requested");
        assert_ne!(incident["diagnosis_turn"], incident["turn"]);
        assert_eq!(
            incident["repair"]["phase"],
            if mode.succeeds() {
                "verified"
            } else {
                "blocked"
            },
            "{incident}"
        );
        let health = herdr_flow::watch_service::health(&guard);
        assert_eq!(health["healthy"], mode.succeeds());
        if mode.succeeds() {
            assert_ne!(health["pid"], watcher);
            let activation = util::read_state_json(&guard, "activation.json").unwrap();
            assert_eq!(activation["pid"], health["pid"]);
            assert_eq!(activation["session"], state["session"]);
            assert_eq!(activation["recovery_incident"], incident["id"]);
            assert_eq!(incident["repair"]["watcher"]["pid"], health["pid"]);
            assert_eq!(
                incident["repair"]["original_operation"]["binding"]["turn"],
                original["turn"]
            );
            assert_eq!(incident["repair"]["operation_replay_allowed"], false);
            if let Some(pid) = checkpoint_pid {
                assert_eq!(health["pid"], pid);
            }
        }
    }
    if mode == Mode::Unknown {
        assert_eq!(
            incident["repair"]["error"],
            "unresolved tool operation blocks receiver recovery",
            "{}",
            fixture_evidence(&coordinator, &mut broker, &server)
        );
        assert_eq!(
            util::read_state_json(&coordinator, "control-tools.json").unwrap(),
            unknown
        );
    }
    assert_eq!(
        util::read_state_json(&coordinator, "receiver-start.json").unwrap(),
        original
    );
    assert_eq!(
        util::read_state_json(&coordinator, "receiver.json").unwrap(),
        registration
    );
    assert_eq!(incident["agent"]["session"], RECOVERY);
    assert_ne!(incident["agent"]["session"], state["session"]);
    assert!(f.state()["tasks"]["task"]["final_outcome"].is_null());
    assert!(workspace.exists());
    let status = Command::new(env!("CARGO_BIN_EXE_herdr-flow"))
        .args([
            "task-runtime",
            "recovery-status",
            "--task",
            "task",
            "--state-dir",
        ])
        .arg(&coordinator)
        .output()
        .unwrap();
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );
    drop(env);
    // A later observation of the same stopped watcher must not send another
    // recovery turn or another coordinator notification.
    if !mode.succeeds() {
        std::thread::sleep(Duration::from_millis(2300));
    }
    let calls = requests.lock().unwrap().clone();
    if mode.requests_repair() {
        let handoff = calls
            .iter()
            .rev()
            .find(|r| r["method"] == "turn/start" && r["params"]["threadId"] == COORD)
            .unwrap();
        let text = handoff["params"]["input"][0]["text"].as_str().unwrap();
        assert!(text.contains(&format!("Repair record: {}", incident["repair"])));
    }
    assert_eq!(
        calls
            .iter()
            .filter(|r| r["method"] == "thread/start")
            .count(),
        2
    );
    assert_eq!(
        calls
            .iter()
            .filter(|r| r["method"] == "turn/start" && r["params"]["threadId"] == RECOVERY)
            .count(),
        if mode.requests_repair() { 2 } else { 1 }
    );
    assert_eq!(
        calls
            .iter()
            .filter(|r| r["method"] == "turn/start" && r["params"]["threadId"] == COORD)
            .count(),
        if failed { 1 } else { 2 }
    );
    let observer = coordinator.join("recovery-observer");
    let identity = util::read_state_json(&observer, "watch.json").unwrap();
    let observer_pid = identity["pid"].as_u64().unwrap() as u32;
    let observer_ticks = identity["start_ticks"].as_str().unwrap();
    upstream_runtime::finalize_with_retry(&f);
    // A final outcome closes observation before resource teardown; no new
    // diagnosis or replay may escape this terminal boundary. A closing Owner
    // also refuses new watch.json writes, so check actual process identity.
    let stopped_by = Instant::now() + Duration::from_secs(5);
    while util::process::process_start_ticks(observer_pid).as_deref() == Some(observer_ticks) {
        assert!(
            Instant::now() < stopped_by,
            "observer did not stop after finalization"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    let cleanup = queue::finalization::drive(&f.queue).unwrap();
    assert_eq!(cleanup.pending, 0, "{cleanup:?}");
    broker.wait().unwrap();
    assert!(!workspace.exists() && !scratch.exists());
    assert!(!task_env::is_registered("task").unwrap());
    assert!(server.path.exists());
    assert_eq!(fs::read_to_string(config).unwrap(), "must remain unchanged");
    server.finish();
    let calls = requests.lock().unwrap();
    for session in [COORD, RECOVERY] {
        assert!(calls
            .iter()
            .any(|r| r["method"] == "thread/delete" && r["params"]["threadId"] == session));
    }
}

fn hold_repair_lock(coordinator: &Path, guard: &Path, mode: Mode) -> Option<util::FileLock> {
    let path = match mode {
        Mode::ControlBusy => coordinator.join("control-service.lock"),
        Mode::WatchBusy => guard.join("watch.lock"),
        _ => return None,
    };
    if mode == Mode::WatchBusy {
        // Do not make the old watcher's released lock look held until its
        // actual outage has been recorded by the independent observer.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let incident = read_incident(coordinator);
            if incident["fault"]["source"] == json!(guard) {
                break;
            }
            assert!(Instant::now() < deadline, "{incident}");
            std::thread::sleep(Duration::from_millis(25));
        }
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    let held = loop {
        if let Some(lock) = util::FileLock::acquire(&path, false).unwrap() {
            break lock;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    };
    if mode == Mode::ControlBusy {
        util::atomic_write_json(
            &coordinator.join("record-checkpoint.json"),
            &json!({"phase":"pruning","generation":1,"active":{},"files":{}}),
        )
        .unwrap();
    }
    Some(held)
}

fn read_incident(coordinator: &Path) -> J {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let status = task_runtime::recovery::status(coordinator).unwrap();
        if status["phase"] != "updating" {
            return status["incident"].clone();
        }
        assert!(Instant::now() < deadline, "{status}");
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn restart_from_spawn_checkpoint(coordinator: &Path) -> u64 {
    let root = coordinator.join("recovery-observer");
    let deadline = Instant::now() + Duration::from_secs(30);
    let (lock, mut incident) = loop {
        if let Ok(lock) = guard::locked_existing(&root) {
            if let Some(incident) =
                util::read_state_json_bounded_if_exists(&root, "incident.json", 1024 * 1024)
                    .unwrap()
            {
                if incident["phase"] == "pending" && incident["repair"]["phase"] == "verified" {
                    break (lock, incident);
                }
                assert!(
                    incident["diagnosis"].is_null() || incident["phase"] != "submitted",
                    "verification started before the saved checkpoint: {incident}"
                );
            }
        }
        assert!(
            Instant::now() < deadline,
            "repair did not reach its saved spawn checkpoint"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    let watch = util::read_state_json(&root, "watch.json").unwrap();
    let old_pid = watch["pid"].as_u64().unwrap() as u32;
    assert!(Command::new("kill")
        .args(["-TERM", &old_pid.to_string()])
        .status()
        .unwrap()
        .success());
    while util::process::process_start_ticks(old_pid).as_deref() == watch["start_ticks"].as_str() {
        assert!(Instant::now() < deadline, "old observer did not exit");
        std::thread::sleep(Duration::from_millis(10));
    }
    // Restore the durable checkpoint immediately after registration. There has
    // been no verification turn or handoff yet; the real watcher remains live.
    let watcher = incident["repair"]["watcher"]["pid"].as_u64().unwrap();
    incident["phase"] = json!("repairing");
    incident["repair"]["phase"] = json!("started");
    util::atomic_write_json(&root.join("incident.json"), &incident).unwrap();
    restart_observer(&root);
    // The restarted observer must not reconcile until its registration helper
    // has released the environment handle. While this incident lock is held it
    // can publish heartbeats but cannot mistake fixture setup for a repair fault.
    drop(lock);
    watcher
}

fn restart_observer(root: &Path) {
    use std::os::unix::process::CommandExt;
    let mut env = task_env::restore_environment("task").unwrap();
    let owner = task_env::isolation::owner(&env).unwrap();
    let _permit = lifecycle::write_permit(&owner).unwrap();
    // This fixture restarts only its registered host observer, retaining the
    // original lifecycle permit through spawn/registration. No model is run.
    let mut command = Command::new(env!("CARGO_BIN_EXE_herdr-flow"));
    command
        .env(task_env::MARKER_ENV, &env.instance_id)
        .env(
            "HERDR_LIFECYCLE_OWNER",
            serde_json::to_string(&owner).unwrap(),
        )
        .env("TMPDIR", &env.temp_root)
        .process_group(0);
    let child = command
        .args(["task-runtime", "recovery-watch", "--state-dir"])
        .arg(root)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    env.register_process(child.id() as i32).unwrap();
    util::process::BackgroundChild {
        pid: child.id(),
        child,
        ready: None,
    }
    .reap();
    // Force an observer tick while this registration handle still owns the
    // environment. The caller's incident lock must defer reconciliation.
    observer_heartbeats_while_repair_is_locked(root);
}

fn observer_heartbeats_while_repair_is_locked(root: &Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut previous = None;
    loop {
        let health = herdr_flow::watch_service::health(root);
        if health["healthy"] == true {
            let current = health["heartbeat_at"].as_f64().unwrap();
            if previous.is_some_and(|before| current > before) {
                break;
            }
            previous = Some(current);
        }
        assert!(Instant::now() < deadline, "{health}");
        std::thread::sleep(Duration::from_millis(25));
    }
    let incident = util::read_state_json(root, "incident.json").unwrap();
    assert_eq!(incident["phase"], "repairing", "{incident}");
    assert_eq!(incident["repair"]["phase"], "started", "{incident}");
}

fn status_reader_does_not_stop_observer(coordinator: &Path) {
    let observer = coordinator.join("recovery-observer");
    let deadline = Instant::now() + Duration::from_secs(10);
    let _reader = loop {
        match guard::locked_existing(&observer) {
            Ok(lock) => break lock,
            Err(error) if error.kind == herdr_flow::ErrorKind::StateBusy => {
                assert!(Instant::now() < deadline, "{error}");
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(error) => panic!("{error}"),
        }
    };
    let mut heartbeat = util::read_state_json(&observer, "watch.json").unwrap()["heartbeat_at"]
        .as_f64()
        .unwrap();
    // Two heartbeats exclude a tick that had released its lock immediately
    // before this reader acquired it. The next tick must survive contention.
    for _ in 0..2 {
        loop {
            let health = herdr_flow::watch_service::health(&observer);
            assert_eq!(health["healthy"], true, "{health}");
            let next = health["heartbeat_at"].as_f64().unwrap();
            if next > heartbeat {
                heartbeat = next;
                break;
            }
            assert!(Instant::now() < deadline, "{health}");
            std::thread::sleep(Duration::from_millis(25));
        }
    }
    assert_eq!(
        task_runtime::recovery::status(coordinator).unwrap()["phase"],
        "updating"
    );
}

fn serve(
    mut peer: rpc_server::Peer,
    mut connection: fixture_server::Connection,
    cwd: &Path,
    requests: &Mutex<Vec<J>>,
    mode: Mode,
) {
    peer.initialize();
    let mut turns = json!([]);
    while let Some(request) = connection.next(&mut peer) {
        requests.lock().unwrap().push(request.clone());
        if mode == Mode::Failure
            && request["method"] == "turn/start"
            && request["params"]["threadId"] == RECOVERY
        {
            peer.json(&json!({"id":request["id"],"error":{"code":-32000,"message":"synthetic unavailable model"}}));
            continue;
        }
        let result = response(&request, cwd, &mut turns, mode);
        peer.json(&json!({"id":request["id"],"result":result}));
        if request["method"] == "thread/delete" {
            break;
        }
    }
}

fn response(request: &J, cwd: &Path, turns: &mut J, mode: Mode) -> J {
    let params = &request["params"];
    match request["method"].as_str().unwrap() {
        "thread/start" => {
            let recovery = params["model"] == "diagnostic";
            if recovery {
                assert_eq!(params["sandbox"], "read-only");
                assert_eq!(params["dynamicTools"], json!([]));
                assert_eq!(params["config"]["model_reasoning_effort"], "medium");
            }
            json!({"thread":{"id":if recovery {RECOVERY} else {COORD},"cwd":cwd,
                "modelProvider":"existing","turns":[]},"model":params["model"],"modelProvider":"existing",
                "reasoningEffort":if recovery {"medium"} else {"high"}})
        }
        "thread/read" => json!({"thread":{"id":params["threadId"],"cwd":cwd,
            "modelProvider":"existing","status":{"type":"idle"},"canAcceptDirectInput":true,"turns":turns}}),
        "thread/turns/list" => {
            json!({"data":[{"id":"coordinator-turn","status":"completed","items":[]}],"nextCursor":null})
        }
        "turn/start" => {
            if params["threadId"] == RECOVERY {
                assert_eq!(params["sandboxPolicy"]["type"], "readOnly");
                let verifying = !turns.as_array().unwrap().is_empty();
                let requested = mode.requests_repair() && !verifying;
                if verifying {
                    let text = params["input"][0]["text"].as_str().unwrap();
                    assert!(text.contains("another repair request is not allowed"));
                    assert!(text.contains(if mode.succeeds() {
                        "verified"
                    } else {
                        "blocked"
                    }));
                }
                let result = json!({"incident":params["outputSchema"]["properties"]["incident"]["enum"][0],
                    "disposition":if requested {"repair_requested"} else if mode.succeeds() {"recovered"} else {"needs_human"},
                    "requested_action":if requested {"restart_receiver_watch"} else {"none"},"facts":"Recorded watcher stopped",
                    "actions":"Read the original task evidence","current_state":"Original task retained",
                    "next_step":"Inspect original operations before restarting the exact watcher"});
                let id = if verifying {
                    "verification-turn"
                } else {
                    "diagnostic-turn"
                };
                turns
                    .as_array_mut()
                    .unwrap()
                    .push(json!({"id":id,"status":"completed",
                    "items":[{"type":"agentMessage","text":result.to_string()}]}));
                json!({"turn":{"id":id}})
            } else {
                json!({"turn":{"id":"coordinator-turn"}})
            }
        }
        "thread/delete" => json!({}),
        "thread/list" => json!({"data":[],"nextCursor":null}),
        method => panic!("unexpected {method}"),
    }
}

fn fixture_evidence(
    coordinator: &Path,
    broker: &mut std::process::Child,
    server: &fixture_server::Server,
) -> J {
    let runtime = util::read_state_json(coordinator, "runtime.json").unwrap_or(J::Null);
    let pid = runtime["pid"].as_u64().unwrap_or_default() as u32;
    json!({"runtime_pid":pid,"runtime_start_time":runtime["start_time"],
        "observed_start_ticks":util::process_start_ticks(pid),
        "broker_exit":broker.try_wait().unwrap().map(|s|s.to_string()),
        "broker_status":util::read_state_json(coordinator,"broker-status.json").ok(),
        "broker_log":util::read_file_limited(&coordinator.join("broker.log"),32*1024).ok().map(|b|String::from_utf8_lossy(&b).to_string()),
        "connections":server.snapshot()})
}

#[test]
fn idle_observation_does_not_disconnect_the_owned_broker() {
    let f = Fixture::new();
    let mut env = isolated_runtime::linked(&f);
    let coordinator = task_runtime::prepare(&env, "coordinator").unwrap();
    let cwd = env.workspace_root.join("checkout");
    let server_cwd = cwd.clone();
    let (send, receive) = std::sync::mpsc::channel();
    let receive = Mutex::new(receive);
    let server = fixture_server::Server::start(
        1,
        Duration::from_millis(40),
        move |mut peer, mut connection| {
            peer.initialize();
            let request = peer.request();
            assert_eq!(request["method"], "thread/start");
            let result = response(&request, &server_cwd, &mut json!([]), Mode::Diagnose);
            peer.json(&json!({"id":request["id"],"result":result}));
            receive
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(3))
                .unwrap();
            assert!(connection.next(&mut peer).is_none());
        },
    );
    let mut broker=task_runtime::launch(&mut env,&coordinator,&json!({"adapter":"codex","configuration":"upstream","socket":server.path,"cwd":cwd,"model":{"providerID":"existing","id":"coord","reasoning_effort":"high"},"driver_binary":env!("CARGO_BIN_EXE_herdr-flow")})).unwrap();
    task_runtime::native::receiver::bootstrap(
        &env,
        &coordinator,
        &json!({"guard_dir":coordinator.join("guard"),"task":"task","role":"coordinator"}),
    )
    .unwrap();
    send.send(()).unwrap();
    let deadline = Instant::now() + Duration::from_millis(300);
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    let before = fixture_evidence(&coordinator, &mut broker, &server);
    let records = server.finish();
    let exit = await_fixture_broker_exit(&mut broker);
    eprintln!(
        "fixture idle/shutdown evidence: {}",
        json!({"before":before,"closed_connections":records,"exit_after_shutdown":exit.map(|s|s.to_string())})
    );
    assert!(
        before["broker_exit"].is_null(),
        "idle observation terminated broker: {before}"
    );
    assert!(
        records[0]["idle_observations"].as_u64().unwrap() >= 2,
        "{records}"
    );
    assert_eq!(records[0]["close_reason"], "explicit_shutdown", "{records}");
    assert!(
        exit.is_some(),
        "explicit fixture shutdown did not stop broker: {before}"
    );
}

fn await_fixture_broker_exit(broker: &mut std::process::Child) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let exit = broker.try_wait().unwrap();
        if exit.is_some() {
            return exit;
        }
        if Instant::now() >= deadline {
            broker.kill().unwrap();
            broker.wait().unwrap();
            return None;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn recovery_fixture_drop_interrupts_an_idle_connection_during_unwind() {
    let server =
        fixture_server::Server::start(1, Duration::from_millis(40), |mut peer, mut connection| {
            peer.initialize();
            assert!(connection.next(&mut peer).is_none());
        });
    let mut client = herdr_flow::codex::rpc::Client::new(server.path.to_str().unwrap());
    client.connect().unwrap();
    let started = Instant::now();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        let _server = server;
        panic!("exercise fixture owner unwind");
    }));
    assert!(result.is_err());
    assert!(started.elapsed() < Duration::from_secs(3));
    assert!(client
        .call("thread/read", &json!({"threadId":"unused"}))
        .is_err());
}

#[test]
fn recovery_fixture_records_actual_peer_eof() {
    let server =
        fixture_server::Server::start(1, Duration::from_millis(40), |mut peer, mut connection| {
            peer.initialize();
            assert!(connection.next(&mut peer).is_none());
        });
    let mut client = herdr_flow::codex::rpc::Client::new(server.path.to_str().unwrap());
    client.connect().unwrap();
    client.close(false);
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let records = server.snapshot();
        if records[0]["close_reason"] == "eof" {
            break;
        }
        assert!(Instant::now() < deadline, "{records}");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(server.finish()[0]["close_reason"], "eof");
}

#[test]
fn recovery_fixture_handler_failure_still_joins_other_idle_peers() {
    let count = std::sync::atomic::AtomicUsize::new(0);
    let server = fixture_server::Server::start(
        2,
        Duration::from_millis(40),
        move |mut peer, mut connection| {
            peer.initialize();
            if count.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                panic!("exercise handler failure");
            }
            assert!(connection.next(&mut peer).is_none());
        },
    );
    let mut first = herdr_flow::codex::rpc::Client::new(server.path.to_str().unwrap());
    first.connect().unwrap();
    let mut second = herdr_flow::codex::rpc::Client::new(server.path.to_str().unwrap());
    second.connect().unwrap();
    let started = Instant::now();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || server.finish()));
    assert!(result.is_err());
    assert!(started.elapsed() < Duration::from_secs(3));
    assert!(second
        .call("thread/read", &json!({"threadId":"unused"}))
        .is_err());
}
