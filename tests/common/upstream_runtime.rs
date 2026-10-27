//! The production attach/broker/finalizer against a shared protocol service.
use super::*;
use std::sync::mpsc;

const OWNED: &str = "22222222-2222-2222-2222-222222222222";
const FOREIGN: &str = "33333333-3333-3333-3333-333333333333";

#[test]
fn upstream_thread_cleanup_preserves_service_config_and_other_sessions() {
    for lost_reply in [false, true] {
        exercise(lost_reply, 0);
    }
}

#[test]
fn same_receiver_crosses_64_turns_rejects_retired_apply_and_cleans_up() {
    exercise(false, 70);
}

fn exercise(lost_reply: bool, continuation_rounds: usize) {
    let f = Fixture::new();
    let mut env = isolated_runtime::linked(&f);
    let root = task_runtime::prepare(&env, "shared").unwrap();
    let cwd = env.workspace_root.join("checkout");
    let (release, wait) = mpsc::channel();
    let (tool_reply, received) = mpsc::channel();
    let server_cwd = cwd.clone();
    let server_queue = f.queue.clone();
    let mut index = 0;
    // The shared service must survive all durable cleanup phases before the
    // next deletion/retry connection; it is not a 15-second one-shot RPC peer.
    let server = rpc_server::Server::start_connections_with_timeout(
        if lost_reply { 5 } else { 4 },
        std::time::Duration::from_secs(60),
        move |mut peer| {
            peer.initialize();
            let request = peer.request();
            if request["method"] == "thread/list" {
                peer.json(&json!({"id":request["id"],"result":{"data":[],"nextCursor":null}}));
                let next = peer.request();
                assert_eq!(next["method"], "thread/list");
                peer.json(&json!({"id":next["id"],"result":{"data":[],"nextCursor":null}}));
                return;
            }
            if index == 0 {
                assert_eq!(request["method"], "thread/start");
                peer.json(&json!({"id":request["id"],"result":{
                "thread":{"id":OWNED,"cwd":server_cwd,"modelProvider":"openai","turns":[]},
                "model":"fixture","modelProvider":"openai"}}));
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
                let mut turns = 0;
                let mut replies = 0;
                let mut retired_apply = J::Null;
                while wait.try_recv().is_err() {
                    assert!(std::time::Instant::now() < deadline);
                    if !readable(&peer) {
                        continue;
                    }
                    if wait.try_recv().is_ok() {
                        break;
                    }
                    let request = peer.request();
                    if request["method"].is_null() {
                        replies += 1;
                        if replies == 4 || turns > 1 {
                            let id = if turns == 1 {
                                "fixture-turn".to_owned()
                            } else {
                                format!("fixture-turn-{turns}")
                            };
                            peer.json(&json!({"method":"turn/completed","params":{"threadId":OWNED,"turn":{"id":id,"status":"completed"}}}));
                        }
                        tool_reply.send(request).unwrap();
                        continue;
                    }
                    if request["method"] == "turn/start" {
                        turns += 1;
                    }
                    let result = match request["method"].as_str().unwrap() {
                        "thread/read" => json!({"thread":{"id":OWNED,"cwd":server_cwd,
                            "modelProvider":"openai","status":{"type":"idle"},"canAcceptDirectInput":true}}),
                        "turn/start" => {
                            json!({"turn":{"id":if turns == 1 { "fixture-turn".to_owned() } else { format!("fixture-turn-{turns}") }}})
                        }
                        method => panic!("unexpected {method}"),
                    };
                    assert_eq!(request["params"]["threadId"], OWNED);
                    peer.json(&json!({"id":request["id"],"result":result}));
                    if request["method"] == "turn/start" {
                        if turns > 1 {
                            peer.json(&json!({"id":1000+turns,"method":"item/tool/call","params":{
                                "threadId":OWNED,"turnId":format!("fixture-turn-{turns}"),"callId":format!("call-{turns}"),
                                "tool":"herdr_queue","arguments":{"action":"inspect","delivery":"","spec":""}}}));
                            if turns == continuation_rounds + 1 {
                                peer.json(&retired_apply);
                            }
                            continue;
                        }
                        for (id, thread) in [(100, OWNED), (101, FOREIGN)] {
                            peer.json(&json!({"id":id,"method":"item/tool/call","params":{
                                "threadId":thread,"turnId":"fixture-turn","callId":format!("call-{id}"),
                                "tool":"herdr_queue","arguments":{"action":"inspect","delivery":"","spec":""}}}));
                        }
                        let q = queue::load_queue(&server_queue).unwrap();
                        let spec = json!({"task":"dev","state_version":q["tasks"]["dev"]["state_version"],"event":"synthetic-result",
                            "next_action":{"kind":"blocked","owner":"coordinator","reason":"synthetic failure","condition":"inspected recovery"}});
                        for (id, args) in [
                            (
                                102,
                                json!({"action":"ack","delivery":q["active"],"spec":""}),
                            ),
                            (
                                103,
                                json!({"action":"apply","delivery":"","spec":spec.to_string()}),
                            ),
                        ] {
                            let tool = json!({"id":id,"method":"item/tool/call","params":{"threadId":OWNED,"turnId":"fixture-turn",
                                "callId":format!("call-{id}"),"tool":"herdr_queue","arguments":args}});
                            if id == 103 {
                                retired_apply = tool.clone();
                            }
                            peer.json(&tool);
                        }
                    }
                }
            } else if index == 1 && lost_reply {
                assert_eq!(request["method"], "thread/delete");
                assert_eq!(request["params"]["threadId"], OWNED);
                // The server applied deletion, but its response was lost.
            } else if index == 1 || (lost_reply && index == 2) {
                assert_eq!(request["method"], "thread/delete");
                assert_eq!(request["params"]["threadId"], OWNED);
                if lost_reply {
                    peer.json(&json!({"id":request["id"],"error":{"code":-32600,"message":format!("thread not found: {OWNED}")}}));
                } else {
                    peer.json(&json!({"id":request["id"],"result":{}}));
                }
            } else {
                assert_eq!(request["method"], "thread/read");
                assert_eq!(request["params"]["threadId"], FOREIGN);
                peer.json(&json!({"id":request["id"],"result":{"thread":{"id":FOREIGN}}}));
            }
            index += 1;
        },
    );
    let config = server.path.parent().unwrap().join("user-config");
    fs::write(&config, b"fixture config must survive unchanged").unwrap();
    let mut broker = task_runtime::launch(
        &mut env,
        &root,
        &json!({
            "adapter":"codex","configuration":"upstream","socket":server.path,
            "cwd":cwd,"model":{"providerID":"openai","id":"fixture"},
            "driver_binary":env!("CARGO_BIN_EXE_herdr-flow")
        }),
    )
    .unwrap();
    // A separate task broker is owned; the server/test process is not.
    assert!(env.owns_process(broker.id() as i32));
    assert!(!env.owns_process(std::process::id() as i32));
    broker.kill().unwrap();
    broker.wait().unwrap();
    task_runtime::start_broker(
        &mut env,
        &root,
        &json!({"driver_binary":env!("CARGO_BIN_EXE_herdr-flow")}),
    )
    .unwrap();
    let restarted = task_runtime::verify(&env, &root).unwrap();
    assert_ne!(restarted["pid"], broker.id());
    assert_eq!(restarted["configuration"], "upstream");

    let state = task_runtime::native::receiver::bootstrap(
        &env,
        &root,
        &json!({
            "guard_dir":root.join("guard"),"task":"task","role":"coordinator"
        }),
    )
    .unwrap();
    assert_eq!(state["session"], OWNED);
    task_runtime::native::receiver::active::activate(
        &mut env,
        &root,
        &json!({"driver_binary":env!("CARGO_BIN_EXE_herdr-flow")}),
    )
    .unwrap();
    assert!(task_runtime::native::receiver::active::activate(&mut env, &root, &json!({})).is_err());
    let active = util::read_state_json(&root.join("guard"), "state.json").unwrap();
    assert_eq!(
        task_runtime::native::receiver::active::observe(&root.join("guard")).unwrap()
            ["interactive_ready"],
        true
    );
    {
        let _lock = queue::locked(&f.queue, false).unwrap();
        let mut q = f.state();
        q["guard_dir"] = json!(root.join("guard"));
        q["receiver"] = guard::scope(&active);
        q["notify"] = json!(true);
        let wait = q["waits"]["dev"].clone();
        let mut event = json!({"id":"synthetic-result","task":"dev","wait":"dev","epoch":1,"acknowledged":false,"outcome":"failed"});
        progress::attach(&mut q, &mut event, &wait, util::now()).unwrap();
        q["events"]["synthetic-result"] = event;
        queue::save(&f.queue, &mut q).unwrap();
    }
    let adapter = herdr_flow::herdr::receiver_factory(&active).unwrap();
    assert_eq!(adapter.agent().unwrap()["interactive_ready"], true);
    {
        let _lock = queue::locked(&f.queue, false).unwrap();
        queue::tick(&f.queue, &herdr_flow::herdr::receiver_factory, None).unwrap();
    }
    // Four durable tool transactions share CPU/IO with process-lifecycle tests.
    // Keep a bounded deadline below the server's, and require every real reply.
    let started = std::time::Instant::now();
    let deadline = started + std::time::Duration::from_secs(15);
    let mut responses = vec![];
    while responses.len() < 4 {
        {
            let _lock = queue::locked(&f.queue, false).unwrap();
            queue::tick(&f.queue, &herdr_flow::herdr::receiver_factory, None).unwrap();
        }
        responses.extend(received.try_iter());
        assert!(
            std::time::Instant::now() < deadline,
            "tool bridge timeout after {:?}: {} of 4 replies; journal: {:?}",
            started.elapsed(),
            responses.len(),
            util::read_state_json(&root, "broker-tools.json")
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    eprintln!(
        "fixture received all four tool replies after {:?}",
        started.elapsed()
    );
    assert_eq!(responses[0]["id"], 100);
    assert_eq!(responses[0]["result"]["success"], true);
    assert_eq!(responses[1]["id"], 101);
    assert_eq!(responses[1]["result"]["success"], false);
    assert_eq!(responses[2]["result"]["success"], true, "{}", responses[2]);
    assert_eq!(responses[3]["result"]["success"], true, "{}", responses[3]);
    assert_eq!(
        f.state()["events"]["synthetic-result"]["acknowledged"],
        true
    );
    assert_eq!(f.state()["events"]["synthetic-result"]["applied"], true);
    let completed = util::read_state_json(&root, "control-tools.json").unwrap();
    {
        let _lock = queue::locked(&f.queue, false).unwrap();
        queue::tick(&f.queue, &herdr_flow::herdr::receiver_factory, None).unwrap();
    }
    assert_eq!(
        util::read_state_json(&root, "control-tools.json").unwrap(),
        completed
    );
    let identity = f.state()["tasks"]["task"]["environment"].clone();
    let applied_version = f.state()["tasks"]["dev"]["state_version"].clone();
    if continuation_rounds > 0 {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let service = loop {
            if let Some(lock) =
                util::FileLock::acquire(&root.join("control-service.lock"), false).unwrap()
            {
                break lock;
            }
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        let _lock = queue::locked(&f.queue, false).unwrap();
        // A checkpoint holding the service lock is ordinary backpressure:
        // the queue tick must survive and try the pending requests next time.
        queue::tick(&f.queue, &herdr_flow::herdr::receiver_factory, None).unwrap();
        drop(service);
    }
    for round in 0..continuation_rounds {
        adapter
            .prompt("Inspect the same task once, then stop.")
            .unwrap();
        let count = if round + 1 == continuation_rounds {
            2
        } else {
            1
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut replies = Vec::new();
        while replies.len() < count {
            {
                let _lock = queue::locked(&f.queue, false).unwrap();
                queue::tick(&f.queue, &herdr_flow::herdr::receiver_factory, None).unwrap();
            }
            replies.extend(received.try_iter());
            assert!(
                std::time::Instant::now() < deadline,
                "continuation {round}: {replies:?}"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        for reply in replies {
            if reply["id"] == 103 {
                assert_eq!(
                    reply["result"]["success"], false,
                    "retired apply replayed: {reply}"
                );
                assert!(reply["result"]["contentItems"][0]["text"]
                    .as_str()
                    .unwrap()
                    .contains("previous turn"));
            } else {
                assert_eq!(reply["result"]["success"], true, "{reply}");
            }
        }
        assert_eq!(f.state()["tasks"]["task"]["environment"], identity);
        assert_eq!(f.state()["tasks"]["dev"]["state_version"], applied_version);
    }
    if continuation_rounds > 0 {
        let checkpoint = util::read_state_json(&root, "record-checkpoint.json").unwrap();
        assert!(
            checkpoint["generation"].as_u64().unwrap() >= 64,
            "{checkpoint}"
        );
        let control = util::read_state_json(&root, "control-tools.json").unwrap();
        assert!(control.as_object().unwrap().len() < 10, "{control}");
    }
    assert!(adapter.call(&["notify".into()]).is_err());
    let mut wrong = active;
    wrong["session"] = json!(FOREIGN);
    assert!(herdr_flow::herdr::receiver_factory(&wrong).is_err());

    assert!(fs::read_dir(root.join("c")).unwrap().next().is_none());
    release.send(()).unwrap();
    drop(env);
    finalize_with_retry(&f);
    // Model a watcher observation holding its shared lifecycle permit. Terminal
    // cleanup must leave the obligation intact and continue after release.
    let before = f.state();
    let owner = lifecycle::task_owner(&before, "task").unwrap().unwrap();
    let observation = lifecycle::acquire(&owner, false).unwrap();
    let report = queue::finalization::drive(&f.queue).unwrap();
    assert_eq!(report.pending, 1, "{report:?}");
    assert_eq!(report.errors, [LIFECYCLE_BUSY]);
    assert_eq!(f.state(), before);
    assert!(root.exists());
    drop(observation);
    let report = retry_cleanup_contention(&f.queue, report);
    if lost_reply {
        assert_eq!(report.pending, 1, "{report:?}");
        assert!(root.exists());
        assert!(task_env::is_registered("task").unwrap());
        let retry =
            retry_cleanup_contention(&f.queue, queue::finalization::drive(&f.queue).unwrap());
        assert_eq!(retry.pending, 0, "{retry:?}");
    } else {
        assert_eq!(report.pending, 0, "{report:?}");
    }
    broker.wait().unwrap();
    assert!(!root.exists());
    assert!(!f.temp.path().join("workspace").exists());
    assert!(!task_env::is_registered("task").unwrap());
    assert_eq!(
        fs::read(&config).unwrap(),
        b"fixture config must survive unchanged"
    );
    let mut client = herdr_flow::codex::rpc::Client::new(server.path.to_str().unwrap());
    assert_eq!(
        client
            .call("thread/read", &json!({"threadId":FOREIGN}))
            .unwrap()["thread"]["id"],
        FOREIGN
    );
    server.finish();
}

const LIFECYCLE_BUSY: &str = "lifecycle action busy; release other locks and retry";

// drive() deliberately performs one bounded attempt. Retry only the known
// pre-mutation permit contention; lost delete replies and other errors must
// reach the scenario assertions instead of being hidden by this helper.
fn retry_cleanup_contention(
    root: &Path,
    mut report: queue::finalization::CleanupReport,
) -> queue::finalization::CleanupReport {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while report.pending == 1 && report.errors == [LIFECYCLE_BUSY] {
        assert!(std::time::Instant::now() < deadline, "{report:?}");
        std::thread::sleep(std::time::Duration::from_millis(20));
        report = queue::finalization::drive(root).unwrap();
    }
    report
}

#[test]
#[ignore = "explicit installed upstream socket; creates and deletes one no-model thread"]
fn live_upstream_identity_and_cleanup_without_model() {
    let socket = std::env::var("HERDR_UPSTREAM_SOCKET").expect("explicit existing socket");
    let model: J =
        serde_json::from_str(&std::env::var("HERDR_UPSTREAM_MODEL").expect("configured model"))
            .unwrap();
    // Preserve all control evidence on panic or partial cleanup. This test runs
    // alone in a subprocess; it never drops a live task's TempDir on failure.
    let f = std::mem::ManuallyDrop::new(Fixture::new());
    eprintln!("live evidence root: {}", f.temp.path().display());
    let mut env = f.ready();
    let root = task_runtime::prepare(&env, "upstream-no-model").unwrap();
    let cwd = env.workspace_root.join("checkout");
    let mut broker = task_runtime::launch(
        &mut env,
        &root,
        &json!({
            "adapter":"codex","configuration":"upstream","socket":socket,"cwd":cwd,"model":model,
            "driver_binary":env!("CARGO_BIN_EXE_herdr-flow")
        }),
    )
    .unwrap();
    let state = task_runtime::native::receiver::bootstrap(
        &env,
        &root,
        &json!({
            "guard_dir":root.join("guard"),"task":"task","role":"coordinator"
        }),
    )
    .unwrap();
    task_runtime::native::receiver::active::activate(
        &mut env,
        &root,
        &json!({"driver_binary":env!("CARGO_BIN_EXE_herdr-flow")}),
    )
    .unwrap();
    util::atomic_write_json(
        &f.temp.path().join("observed.json"),
        &json!({
            "session":state["session"],"version":state["server_version"],"model":state["model"],
            "model_request_sent":false,"runtime":root
        }),
    )
    .unwrap();
    drop(env);
    finalize_with_retry(&f);
    let report = queue::finalization::drive(&f.queue).unwrap();
    assert_eq!(report.pending, 0, "{report:?}");
    broker.wait().unwrap();
    assert!(!root.exists());
    assert!(!f.temp.path().join("workspace").exists());
    assert!(!task_env::is_registered("task").unwrap());
    let mut client = herdr_flow::codex::rpc::Client::new(&socket);
    client.connect().unwrap();
    util::atomic_write_json(
        &f.temp.path().join("result.json"),
        &json!({
            "model_request_sent":false,"owned_runtime_removed":true,"owned_workspace_removed":true,
            "registry_released":true,"shared_service_still_available":true,
            "scope":"no-model upstream identity and cleanup; not real-task acceptance"
        }),
    )
    .unwrap();
}

pub(super) fn finalize_with_retry(f: &Fixture) {
    finalize_with_outcome(f, "failure");
}
pub(super) fn finalize_with_outcome(f: &Fixture, outcome: &str) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        match finalize_outcome(&f.queue, outcome) {
            Ok(_) => return,
            Err(error)
                if error.message.contains("lifecycle action busy")
                    && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(error) => panic!("finalization: {error}"),
        }
    }
}

#[test]
#[ignore = "explicit retained acceptance queue only; completes its pending cleanup"]
fn recover_retained_upstream_acceptance() {
    let root = PathBuf::from(std::env::var("HERDR_UPSTREAM_RECOVER").unwrap());
    let queue_root = root.join("queue");
    std::env::set_var(
        "HERDR_TASK_ENV_ROOT",
        queue_root.join("queue.json/task-environments"),
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let saved = queue::load_queue(&queue_root).unwrap();
        if saved["tasks"]["task"].is_null() || saved["tasks"]["task"]["final_outcome"].is_string() {
            break; // A recorded terminal outcome only needs cleanup, never another finalize.
        }
        let result = finalize_registered(&queue_root);
        match result {
            Ok(_) => break,
            Err(error)
                if error.message.contains("lifecycle action busy")
                    && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(error) => panic!("{error}"),
        }
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let report = queue::finalization::drive(&queue_root).unwrap();
        if report.pending == 0 {
            break;
        }
        assert!(
            report.errors.iter().all(
                |e| e.contains("close is not persisted") || e.contains("lifecycle action busy")
            ),
            "{report:?}"
        );
        assert!(std::time::Instant::now() < deadline, "{report:?}");
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(!root.join("workspace").exists());
    assert!(!root.join("scratch").exists());
    assert!(!task_env::is_registered("task").unwrap());
    util::atomic_write_json(
        &root.join("recovered.json"),
        &json!({"task_resources_removed":true}),
    )
    .unwrap();
}

fn readable(peer: &rpc_server::Peer) -> bool {
    use std::os::fd::AsRawFd;
    let mut fd = libc::pollfd {
        fd: peer.stream.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: poll borrows one initialized descriptor for this bounded call.
    let result = unsafe { libc::poll(&mut fd, 1, 20) };
    assert!(result >= 0);
    result > 0 && fd.revents & libc::POLLIN != 0
}

// Administrative teardown by the explicitly invoked acceptance harness. It does
// not claim a model decision, forge a tool-process probe or modify CODEX_THREAD_ID.
fn finalize_registered(root: &Path) -> herdr_flow::Result<J> {
    finalize_outcome(root, "failure")
}
fn finalize_outcome(root: &Path, outcome: &str) -> herdr_flow::Result<J> {
    let _lock = queue::locked(root, false)?;
    let state = queue::load_queue(root)?;
    let owner = lifecycle::task_owner(&state, "task")?.unwrap();
    let receiver = util::read_state_json(
        Path::new(state["guard_dir"].as_str().unwrap()),
        "state.json",
    )?;
    let spec = json!({"task":"task","queue_id":owner.queue_id,"epoch":owner.epoch,
        "instance_id":owner.instance_id,"state_version":state["tasks"]["task"]["state_version"],
        "outcome":outcome,"reason":"explicit acceptance supervisor finalization; success requires confirmed local delivery"});
    queue::task_action(
        root,
        "task-finalize",
        &spec,
        receiver["session"].as_str(),
        &|_| panic!("no model"),
    )
}
