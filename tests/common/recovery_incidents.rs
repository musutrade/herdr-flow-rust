//! Real owned observer/brokers with a synthetic upstream; no user accounts.
use super::*;
use std::sync::Arc;
use std::time::{Duration, Instant};

const COORD: &str = "22222222-2222-2222-2222-222222222222";
const RECOVERY: &str = "44444444-4444-4444-4444-444444444444";

#[test]
fn stopped_watcher_gets_one_independent_recovery_result_and_all_resources_are_reclaimed() {
    exercise(false);
}

#[test]
fn recovery_model_failure_stays_visible_without_recursion_or_replay() {
    exercise(true);
}

fn exercise(failed: bool) {
    let f = Fixture::new();
    let mut env = isolated_runtime::linked(&f);
    let coordinator = task_runtime::prepare(&env, "coordinator").unwrap();
    let cwd = env.workspace_root.join("checkout");
    let requests = Arc::new(Mutex::new(Vec::<J>::new()));
    let recorded = requests.clone();
    let server_cwd = cwd.clone();
    let server = coordinator_continuation::parallel(5, move |peer| {
        serve(peer, &server_cwd, &recorded, failed);
    });
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
    let watcher = activated["pid"].as_u64().unwrap();
    assert!(env.owns_process(watcher as i32));
    assert!(Command::new("kill")
        .args(["-TERM", &watcher.to_string()])
        .status()
        .unwrap()
        .success());
    let workspace = env.workspace_root.clone();
    let scratch = env.temp_root.clone();
    // Deliberately retain the environment lock until the diagnostic result:
    // a stalled original operation must not prevent the independent role.
    let deadline = Instant::now() + Duration::from_secs(30);
    let incident = loop {
        let status = task_runtime::recovery::status(&coordinator).unwrap();
        if status["incident"]["phase"] == if failed { "unavailable" } else { "handled" } {
            break status["incident"].clone();
        }
        assert!(Instant::now() < deadline, "{status}");
        std::thread::sleep(Duration::from_millis(25));
    };
    if failed {
        assert!(incident["result"].is_null());
        assert!(incident["next_step"]
            .as_str()
            .unwrap()
            .contains("before any retry"));
    } else {
        assert_eq!(incident["result"]["disposition"], "needs_human");
    }
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
    std::thread::sleep(Duration::from_millis(2300));
    let calls = requests.lock().unwrap().clone();
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
        1
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

fn serve(mut peer: rpc_server::Peer, cwd: &Path, requests: &Mutex<Vec<J>>, failed: bool) {
    peer.initialize();
    let mut turns = json!([]);
    while let Some(request) = next(&mut peer) {
        requests.lock().unwrap().push(request.clone());
        if failed && request["method"] == "turn/start" && request["params"]["threadId"] == RECOVERY
        {
            peer.json(&json!({"id":request["id"],"error":{"code":-32000,"message":"synthetic unavailable model"}}));
            continue;
        }
        let result = response(&request, cwd, &mut turns);
        peer.json(&json!({"id":request["id"],"result":result}));
        if request["method"] == "thread/delete" {
            break;
        }
    }
}

fn response(request: &J, cwd: &Path, turns: &mut J) -> J {
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
        "turn/start" => {
            if params["threadId"] == RECOVERY {
                assert_eq!(params["sandboxPolicy"]["type"], "readOnly");
                let result = json!({"incident":params["outputSchema"]["properties"]["incident"]["enum"][0],
                    "disposition":"needs_human","facts":"Recorded watcher stopped",
                    "actions":"Read the original task evidence","current_state":"Original task retained",
                    "next_step":"Inspect original operations before restarting the exact watcher"});
                *turns = json!([{"id":"diagnostic-turn","status":"completed",
                    "items":[{"type":"agentMessage","text":result.to_string()}]}]);
                json!({"turn":{"id":"diagnostic-turn"}})
            } else {
                json!({"turn":{"id":"coordinator-turn"}})
            }
        }
        "thread/delete" => json!({}),
        "thread/list" => json!({"data":[],"nextCursor":null}),
        method => panic!("unexpected {method}"),
    }
}

fn next(peer: &mut rpc_server::Peer) -> Option<J> {
    use std::os::fd::AsRawFd;
    let mut fd = libc::pollfd {
        fd: peer.stream.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: poll borrows one initialized descriptor for this bounded call.
    if unsafe { libc::poll(&mut fd, 1, 30000) } <= 0 {
        return None;
    }
    let mut byte = [0u8];
    // SAFETY: byte is writable and the stream owns the descriptor.
    if unsafe { libc::recv(fd.fd, byte.as_mut_ptr().cast(), 1, libc::MSG_PEEK) } <= 0 {
        return None;
    }
    let (opcode, payload) = peer.frame();
    if opcode == 8 {
        return None;
    }
    assert_eq!(opcode, 1);
    Some(serde_json::from_slice(&payload).unwrap())
}
