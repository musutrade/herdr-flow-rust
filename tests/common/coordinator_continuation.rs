//! Synthetic upstream tools through the real watcher, broker and native worker.
use super::*;
use std::collections::BTreeMap;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc,
};
use std::time::{Duration, Instant};

const COORD: &str = "22222222-2222-2222-2222-222222222222";
const GUARDIAN: &str = "55555555-5555-5555-5555-555555555555";
const WORKER: &str = "33333333-3333-3333-3333-333333333333";
const STAGE_REVIEW: &str = "44444444-4444-4444-4444-444444444444";
const OVERALL_REVIEW: &str = "66666666-6666-6666-6666-666666666666";

#[test]
fn coordinator_dispatches_collects_and_finalizes_without_driver_task_mutations() {
    exercise(false, false);
}

#[test]
fn coordinator_develops_reviews_and_delivers_to_predeclared_local_target() {
    exercise(true, false);
}

#[test]
fn coordinator_completes_stage_then_reaccepts_the_same_parent_before_finalizing() {
    exercise(true, true);
}

type AuditGate = fn(&Path, &J, &Path) -> Option<util::FileLock>;

pub(super) fn observer_first_run(gate: AuditGate) {
    exercise_with_observer(false, false, Some(gate));
}

fn exercise(success: bool, staged: bool) {
    exercise_with_observer(success, staged, None);
}

fn exercise_with_observer(success: bool, staged: bool, audit: Option<AuditGate>) {
    let f = Fixture::new();
    let target = f.temp.path().join("output.git");
    if success {
        git(
            f.temp.path(),
            &[
                "clone",
                "--bare",
                f.temp.path().join("repo").to_str().unwrap(),
                target.to_str().unwrap(),
            ],
        );
        git(&target, &["update-ref", "refs/heads/accepted", &f.commit]);
    }
    let mut env =
        isolated_runtime::linked_delivery(&f, "fixture", success.then_some(target.as_path()));
    let coord = task_runtime::prepare(&env, "coordinator").unwrap();
    let cwd = env.workspace_root.join("checkout");
    let (send, commands) = mpsc::channel::<J>();
    let (replies, receive) = mpsc::channel::<J>();
    let commands = Arc::new(Mutex::new(commands));
    let server_cwd = cwd.clone();
    let guardian = f
        .temp
        .path()
        .join(format!("rollout-guardian-{GUARDIAN}.jsonl"));
    if success {
        fs::write(&guardian, format!("{}\n",json!({"type":"session_meta","payload":{
            "id":GUARDIAN,"parent_thread_id":WORKER,"cwd":cwd,"source":{"subagent":{"other":"guardian"}}}}))).unwrap();
    }
    let shared_guardian = guardian.clone();
    let coordinator_sandbox =
        Mutex::new(json!({"type":"workspaceWrite","networkAccess":false,"writableRoots":[]}));
    let threads = Arc::new(Mutex::new(BTreeMap::new()));
    let shared_threads = threads.clone();
    let server = continuation_server(move |peer, stopped| {
        peer_loop(
            peer,
            PeerContext {
                cwd: &server_cwd,
                commands: &commands,
                replies: &replies,
                guardian: &shared_guardian,
                coordinator_sandbox: &coordinator_sandbox,
                threads: &shared_threads,
                stopped: &stopped,
            },
        );
    });
    let mut broker = task_runtime::launch(
        &mut env,
        &coord,
        &json!({
            "adapter":"codex","configuration":"upstream","socket":server.path,
            "cwd":cwd,"model":{"providerID":"openai","id":"fixture","reasoning_effort":"high"},
            "role_models":{"developer":{"providerID":"openai","id":"developer-fixture","reasoning_effort":"medium"},
                "reviewer":{"providerID":"openai","id":"reviewer-fixture","reasoning_effort":"high"}},
            "driver_binary":env!("CARGO_BIN_EXE_herdr-flow")
        }),
    )
    .unwrap();
    let guard = coord.join("guard");
    task_runtime::native::receiver::bootstrap(
        &env,
        &coord,
        &json!({"guard_dir":guard,"task":"task","role":"coordinator"}),
    )
    .unwrap();
    task_runtime::native::receiver::active::activate(
        &mut env,
        &coord,
        &json!({"driver_binary":env!("CARGO_BIN_EXE_herdr-flow")}),
    )
    .unwrap();
    let state = util::read_state_json(&guard, "state.json").unwrap();
    let activation =
        task_runtime::native::receiver::recovery::recover(&mut env, &coord, &json!({})).unwrap();
    assert_eq!(activation["session"], state["session"]);
    {
        let _lock = queue::locked(&f.queue, false).unwrap();
        let mut q = f.state();
        q["guard_dir"] = json!(guard);
        q["receiver"] = guard::scope(&state);
        q["notify"] = json!(true);
        q["poll_seconds"] = json!(1);
        queue::save(&f.queue, &mut q).unwrap();
    }
    let workspace = env.workspace_root.clone();
    if !success {
        broker.kill().unwrap();
        broker.wait().unwrap();
        task_runtime::start_broker(
            &mut env,
            &coord,
            &json!({"driver_binary":env!("CARGO_BIN_EXE_herdr-flow")}),
        )
        .unwrap();
    }
    drop(env);
    let log = fs::File::create(f.temp.path().join("continuation-watch.log")).unwrap();
    let mut watcher = FixtureChild(
        Command::new(env!("CARGO_BIN_EXE_herdr-flow"))
            .args(["queue", "watch", "--state-dir"])
            .arg(&f.queue)
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap(),
    );
    await_condition(|| {
        herdr_flow::watch_service::health(&queue::artifacts(&f.queue).unwrap())["healthy"] == true
    });
    await_condition(|| task_runtime::native::receiver::active::observe(&guard).is_ok());
    {
        let deadline = Instant::now() + Duration::from_secs(10);
        let env = loop {
            match task_env::restore_environment("task") {
                Ok(env) => break env,
                Err(error)
                    if error.kind == herdr_flow::ErrorKind::StateBusy
                        && !coord.join("receiver-start.json").exists()
                        && Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(25))
                }
                Err(error) => panic!("coordinator environment restore: {error}"),
            }
        };
        loop {
            match task_runtime::native::receiver::start(
                &env,
                &coord,
                &json!({"instruction":"synthetic coordinator"}),
            ) {
                Ok(_) => break,
                Err(error)
                    if error.kind == herdr_flow::ErrorKind::StateBusy
                        && !coord.join("receiver-start.json").exists()
                        && Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(25))
                }
                Err(error) => panic!("coordinator start: {error}"),
            }
        }
        assert!(task_runtime::native::receiver::start(
            &env,
            &coord,
            &json!({"instruction":"duplicate"})
        )
        .is_err());
    }
    // Invalid caller-supplied endpoint must fail before creating a worker.
    let bad = tool(
        &send,
        &receive,
        1,
        "run",
        json!({"wait":"dev","instruction":"fixture","socket":"/foreign"}),
    );
    assert_eq!(bad["success"], false);
    let model_override = tool(
        &send,
        &receive,
        8,
        "run",
        json!({
            "wait":"dev","instruction":"fixture","model":{"providerID":"other","id":"other"}
        }),
    );
    assert_eq!(model_override["success"], false);
    assert!(!coord.parent().unwrap().join("dev-a1").exists());
    let denied = tool(
        &send,
        &receive,
        5,
        "finalize",
        json!({"task":"task",
        "state_version":f.state()["tasks"]["task"]["state_version"],"outcome":"success","reason":"unproven success"}),
    );
    assert_eq!(denied["success"], false);
    assert!(f.state()["tasks"]["task"]["final_outcome"].is_null());
    let before = f.state();
    for (id, spec) in [
        (6, json!({"task":"foreign","kind":"candidate"})),
        (7, json!({"task":"task","kind":"revise","authorization":[]})),
    ] {
        assert_eq!(tool(&send, &receive, id, "lineage", spec)["success"], false);
    }
    assert_eq!(f.state()["tasks"], before["tasks"]);
    let (run, audit_error) = if let Some(gate) = audit {
        run_while_observed(&f, &coord, &mut watcher, &send, &receive, gate)
    } else {
        (
            tool(
                &send,
                &receive,
                2,
                "run",
                json!({"wait":"dev","instruction":"synthetic local development","evidence":[]}),
            ),
            None,
        )
    };
    assert_eq!(
        run["success"],
        true,
        "{run}; {}",
        fs::read_to_string(f.temp.path().join("continuation-watch.log")).unwrap()
    );
    await_condition(|| {
        f.state()["events"]
            .as_object()
            .unwrap()
            .values()
            .any(|e| e["task"] == "dev")
    });
    let view = tool(&send, &receive, 3, "inspect", json!({}));
    assert_eq!(view["success"], true);
    let view: J = serde_json::from_str(view["contentItems"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(view["runtime_results"]["2"]["phase"], "signalled");
    assert_eq!(
        fs::read_to_string(cwd.join("developed")).unwrap(),
        "synthetic output\n"
    );
    if success {
        complete_review_delivery(
            &f,
            &send,
            &receive,
            &view["runtime_results"]["2"]["output"],
            &target,
            staged,
        );
        if staged {
            assert_fresh_review_identity(&threads);
        }
    }
    let result = tool(
        &send,
        &receive,
        40,
        "finalize",
        json!({"task":"task",
        "state_version":f.state()["tasks"]["task"]["state_version"],"outcome":if success {"success"} else {"failure"},"reason":"explicit synthetic final decision"}),
    );
    assert_eq!(result["success"], true, "{result}");
    let deadline = Instant::now() + Duration::from_secs(30);
    while !f.state()["tasks"]["task"].is_null() {
        assert!(
            Instant::now() < deadline,
            "cleanup timed out: {}",
            fs::read_to_string(f.temp.path().join("continuation-watch.log")).unwrap()
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(!workspace.exists() && !coord.exists());
    if success {
        assert_eq!(
            git(&target, &["show", "refs/heads/accepted:developed"]),
            "synthetic output"
        );
    }
    assert!(server.path.exists());
    assert!(!task_env::is_registered("task").unwrap());
    // Normal exit flushes subprocess coverage. This stops only the completed
    // test queue, after the finalizer has removed all task-owned resources.
    {
        let lock = loop {
            match queue::locked(&f.queue, false) {
                Ok(lock) => break lock,
                Err(error) if error.kind == herdr_flow::ErrorKind::StateBusy => {
                    std::thread::sleep(Duration::from_millis(25))
                }
                Err(error) => panic!("{error}"),
            }
        };
        let mut q = f.state();
        q["stopped"] = json!(true);
        queue::save(&f.queue, &mut q).unwrap();
        drop(lock);
    }
    await_condition(|| watcher.0.try_wait().unwrap().is_some());
    assert!(watcher.0.wait().unwrap().success());
    broker.wait().unwrap();
    server.finish();
    if audit.is_some() {
        assert_eq!(
            threads.lock().unwrap()[WORKER]
                .turns
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert!(audit_error.is_none(), "{}", audit_error.unwrap_or_default());
    }
    assert!(!guardian.exists());
}

fn run_while_observed(
    f: &Fixture,
    coordinator: &Path,
    watcher: &mut FixtureChild,
    send: &mpsc::Sender<J>,
    receive: &mpsc::Receiver<J>,
    gate: AuditGate,
) -> (J, Option<String>) {
    // Only this offline fixture's ordinary queue watcher is stopped. The
    // original coordinator/broker and pending operation retain their identity.
    watcher.0.kill().unwrap();
    watcher.0.wait().unwrap();
    let env = task_env::restore_environment("task").unwrap();
    let worker = task_runtime::prepare(&env, "dev-a1").unwrap();
    drop(env);
    // Also expose the old presentation open-before-flock schedule. The audit
    // must never acquire this file; the corrected gate is the coordinator's.
    drop(
        util::FileLock::acquire(&worker.join("presentation.lock"), false)
            .unwrap()
            .unwrap(),
    );
    let owner =
        serde_json::to_value(lifecycle::task_owner(&f.state(), "task").unwrap().unwrap()).unwrap();
    let mut held = None;
    await_condition(|| {
        held = gate(coordinator, &owner, &worker);
        held.is_some()
    });
    let before = util::read_state_json(coordinator, "control-tools.json").unwrap();
    let request = json!({"id":2,"method":"item/tool/call","params":{"threadId":COORD,
        "turnId":"coordinator-turn","tool":"herdr_queue","arguments":{"action":"run","delivery":"",
        "spec":json!({"wait":"dev","instruction":"synthetic local development","evidence":[]}).to_string()}}});
    send.send(request.clone()).unwrap();
    await_condition(|| {
        util::read_state_json(coordinator, "broker-tools.json")
            .is_ok_and(|j| j["2"]["phase"] == "pending")
    });
    let once = Command::new(env!("CARGO_BIN_EXE_herdr-flow"))
        .args(["queue", "watch", "--once", "--state-dir"])
        .arg(&f.queue)
        .output()
        .unwrap();
    let after = util::read_state_json(coordinator, "control-tools.json").unwrap();
    let pending = util::read_state_json(coordinator, "broker-tools.json").unwrap();
    let deferred = once.status.success()
        && before == after
        && after.get("2").is_none()
        && pending["2"]["phase"] == "pending"
        && pending["2"]["request"] == request
        && !worker.join("runtime.json").exists()
        && !worker.join("turn.json").exists();
    let error = (!deferred).then(|| format!("observer-first real driver failed to defer: status={:?}, stderr={}, journal={}, pending={}", once.status, String::from_utf8_lossy(&once.stderr), after, pending["2"]));
    drop(held);
    let log = fs::OpenOptions::new()
        .append(true)
        .open(f.temp.path().join("continuation-watch.log"))
        .unwrap();
    *watcher = FixtureChild(
        Command::new(env!("CARGO_BIN_EXE_herdr-flow"))
            .args(["queue", "watch", "--state-dir"])
            .arg(&f.queue)
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap(),
    );
    let reply = receive.recv_timeout(Duration::from_secs(30)).unwrap();
    assert_eq!(reply["id"], 2);
    let value = reply["result"].clone();
    assert_eq!(value["success"], true, "{value}");
    let result: J =
        serde_json::from_str(value["contentItems"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(result["runtime"], json!(worker));
    assert_eq!(result["turn"]["presentation"]["phase"], "not_configured");
    assert_eq!(
        util::read_state_json(&worker, "turn.json").unwrap()["turn"],
        result["turn"]["turn"]
    );
    (value, error)
}

fn tool(send: &mpsc::Sender<J>, receive: &mpsc::Receiver<J>, id: u64, action: &str, spec: J) -> J {
    send.send(json!({"id":id,"method":"item/tool/call","params":{"threadId":COORD,
        "turnId":"coordinator-turn","tool":"herdr_queue","arguments":{"action":action,"delivery":if action == "ack" {spec["delivery"].as_str().unwrap_or("")} else {""},"spec":spec.to_string()}}})).unwrap();
    let reply = receive
        .recv_timeout(Duration::from_secs(30))
        .unwrap_or_else(|error| {
            let registry = PathBuf::from(std::env::var_os("HERDR_TASK_ENV_ROOT").unwrap());
            let fixture = registry.ancestors().nth(3).unwrap();
            panic!(
                "tool {id} ({action}): {error}; watcher: {}",
                fs::read_to_string(fixture.join("continuation-watch.log")).unwrap_or_default()
            );
        });
    assert_eq!(reply["id"], id);
    reply["result"].clone()
}

fn await_condition(mut predicate: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(30);
    while !predicate() {
        assert!(Instant::now() < end, "continuation timed out");
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[derive(Clone)]
struct NativeThread {
    id: String,
    cwd: PathBuf,
    model: String,
    effort: String,
    coordinator: bool,
    turns: J,
    deleted: bool,
}

struct PeerContext<'a> {
    cwd: &'a Path,
    commands: &'a Mutex<mpsc::Receiver<J>>,
    replies: &'a mpsc::Sender<J>,
    guardian: &'a Path,
    coordinator_sandbox: &'a Mutex<J>,
    threads: &'a Mutex<BTreeMap<String, NativeThread>>,
    stopped: &'a AtomicBool,
}

fn peer_loop(mut peer: rpc_server::Peer, context: PeerContext<'_>) {
    use std::os::fd::AsRawFd;
    peer.initialize();
    let mut coordinator = false;
    let mut subscribed = false;
    let deadline = Instant::now() + Duration::from_secs(120);
    while !context.stopped.load(Ordering::SeqCst) {
        assert!(
            Instant::now() < deadline,
            "continuation peer exceeded its finite deadline"
        );
        if coordinator && subscribed {
            if let Ok(command) = context.commands.lock().unwrap().try_recv() {
                peer.json(&command);
            }
        }
        let mut poll = libc::pollfd {
            fd: peer.stream.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: poll owns one valid descriptor for this bounded call.
        if unsafe { libc::poll(&mut poll, 1, 10) } <= 0 {
            continue;
        }
        let mut byte = [0u8];
        // SAFETY: byte is valid and the peer owns its descriptor.
        if unsafe {
            libc::recv(
                peer.stream.as_raw_fd(),
                byte.as_mut_ptr().cast(),
                1,
                libc::MSG_PEEK,
            )
        } <= 0
        {
            break;
        }
        let request = peer.request();
        if request["method"].is_null() {
            context.replies.send(request).unwrap();
            continue;
        }
        if handle_inventory(&mut peer, &request, &context) {
            continue;
        }
        if request["method"] == "thread/delete" {
            delete_thread(&mut peer, &request, &context);
            break;
        }
        let thread = request_thread(&request, &context);
        let Some(mut thread) = thread else {
            peer.json(&json!({"id":request["id"],"error":{"code":-32600,
                "message":format!("thread not found: {}",request["params"]["threadId"].as_str().unwrap())}}));
            continue;
        };
        coordinator = thread.coordinator;
        if matches!(
            request["method"].as_str(),
            Some("thread/start" | "thread/resume")
        ) {
            subscribed = true;
        }
        if request["method"] == "thread/resume" {
            assert_eq!(request["params"], json!({"threadId":thread.id}));
        }
        let result = thread_result(&request, &mut thread, subscribed, &context);
        if matches!(
            request["method"].as_str(),
            Some("thread/start" | "turn/start")
        ) {
            context
                .threads
                .lock()
                .unwrap()
                .insert(thread.id.clone(), thread);
        }
        peer.json(&json!({"id":request["id"],"result":result}));
    }
}

fn request_thread(request: &J, context: &PeerContext<'_>) -> Option<NativeThread> {
    if request["method"] == "thread/start" {
        let thread = new_thread(request);
        assert!(
            !context.threads.lock().unwrap().contains_key(&thread.id),
            "thread/start must not replay"
        );
        return Some(thread);
    }
    let id = request["params"]["threadId"].as_str().unwrap();
    context
        .threads
        .lock()
        .unwrap()
        .get(id)
        .filter(|thread| !thread.deleted)
        .cloned()
}

fn new_thread(request: &J) -> NativeThread {
    let cwd = PathBuf::from(request["params"]["cwd"].as_str().unwrap());
    let coordinator = request["params"]["dynamicTools"]
        .as_array()
        .is_some_and(|a| !a.is_empty());
    let review = cwd.to_string_lossy().contains("/reviews/");
    let id = if coordinator {
        COORD
    } else if review && cwd.to_string_lossy().contains("overall-review") {
        OVERALL_REVIEW
    } else if review {
        STAGE_REVIEW
    } else {
        WORKER
    };
    let model = if coordinator {
        "fixture"
    } else if review {
        "reviewer-fixture"
    } else {
        "developer-fixture"
    };
    let effort = if coordinator || review {
        "high"
    } else {
        "medium"
    };
    assert_eq!(request["params"]["model"], model);
    assert_eq!(
        request["params"]["config"]["model_reasoning_effort"],
        effort
    );
    NativeThread {
        id: id.into(),
        cwd,
        model: model.into(),
        effort: effort.into(),
        coordinator,
        turns: json!([]),
        deleted: false,
    }
}

fn thread_result(
    request: &J,
    thread: &mut NativeThread,
    subscribed: bool,
    context: &PeerContext<'_>,
) -> J {
    match request["method"].as_str().unwrap() {
        "thread/start" | "thread/read" | "thread/resume" => thread_reply(thread, context),
        "turn/start" => start_turn(request, thread, subscribed, context),
        "thread/turns/list" => json!({"data":thread.turns,"nextCursor":null}),
        other => panic!("unexpected method {other}"),
    }
}

fn thread_reply(thread: &NativeThread, context: &PeerContext<'_>) -> J {
    let sandbox = if thread.coordinator {
        context.coordinator_sandbox.lock().unwrap().clone()
    } else {
        json!({"type":"workspaceWrite","networkAccess":false,"writableRoots":[]})
    };
    json!({"thread":{"id":thread.id,"cwd":thread.cwd,"modelProvider":"openai","model":thread.model,
            "status":{"type":"idle"},"canAcceptDirectInput":true,"turns":thread.turns},
        "model":thread.model,"modelProvider":"openai","reasoningEffort":thread.effort,
        "cwd":thread.cwd,"approvalPolicy":"untrusted","sandbox":sandbox})
}

fn start_turn(
    request: &J,
    thread: &mut NativeThread,
    subscribed: bool,
    context: &PeerContext<'_>,
) -> J {
    assert!(
        subscribed,
        "a new broker connection must resume before submitting another turn"
    );
    assert_eq!(request["params"]["threadId"], thread.id);
    assert_eq!(request["params"]["model"], thread.model);
    assert_eq!(request["params"]["effort"], thread.effort);
    if thread.coordinator {
        let mut sandbox = request["params"]["sandboxPolicy"].clone();
        sandbox["writableRoots"] = json!([]);
        *context.coordinator_sandbox.lock().unwrap() = sandbox;
        return json!({"turn":{"id":"coordinator-turn"}});
    }
    if thread.cwd.to_string_lossy().contains("/reviews/") {
        assert_eq!(
            fs::read_to_string(thread.cwd.join("developed")).unwrap(),
            "synthetic output\n"
        );
        fs::write(thread.cwd.join(".herdr-verdict.json"), serde_json::to_vec(&json!({"conclusion":"passed","scope":"file",
            "checks":[{"id":"content","criterion":"goal","source":"fixture","status":"passed","evidence":"read fixed developed file"}],
            "gaps":[],"reuses":[],"resolutions":[]})).unwrap()).unwrap();
    } else {
        develop(&thread.cwd);
    }
    thread.turns = json!([{"id":"worker-turn","status":"completed","items":[]}]);
    json!({"turn":{"id":"worker-turn"}})
}

fn develop(cwd: &Path) {
    fs::write(cwd.join("developed"), "synthetic output\n").unwrap();
    git(cwd, &["add", "developed"]);
    git(
        cwd,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.test",
            "commit",
            "-qm",
            "synthetic development",
        ],
    );
}

fn handle_inventory(peer: &mut rpc_server::Peer, request: &J, context: &PeerContext<'_>) -> bool {
    if request["method"] != "thread/list" {
        return false;
    }
    let rows = if context.guardian.exists()
        && request["params"]["cwd"] == json!(context.cwd)
        && request["params"]["archived"] == false
    {
        json!([{"id":GUARDIAN,"cwd":context.cwd,"path":context.guardian,"parentThreadId":null,"source":{"subAgent":{"other":"guardian"}}}])
    } else {
        json!([])
    };
    peer.json(&json!({"id":request["id"],"result":{"data":rows,"nextCursor":null}}));
    true
}

fn delete_thread(peer: &mut rpc_server::Peer, request: &J, context: &PeerContext<'_>) {
    let id = request["params"]["threadId"].as_str().unwrap();
    if id == GUARDIAN {
        fs::remove_file(context.guardian).unwrap();
    } else {
        let mut threads = context.threads.lock().unwrap();
        let Some(thread) = threads.get_mut(id).filter(|thread| !thread.deleted) else {
            peer.json(&json!({"id":request["id"],"error":{"code":-32600,"message":format!("thread not found: {id}")}}));
            return;
        };
        thread.deleted = true;
    }
    peer.json(&json!({"id":request["id"],"result":{}}));
}

fn checked(send: &mpsc::Sender<J>, receive: &mpsc::Receiver<J>, id: u64, action: &str, spec: J) {
    let result = tool(send, receive, id, action, spec);
    assert_eq!(result["success"], true, "{action}: {result}");
}

fn complete_review_delivery(
    f: &Fixture,
    send: &mpsc::Sender<J>,
    receive: &mpsc::Receiver<J>,
    output: &J,
    target: &Path,
    staged: bool,
) {
    await_condition(|| !f.state()["active"].is_null());
    checked(
        send,
        receive,
        10,
        "ack",
        json!({"delivery":f.state()["active"]}),
    );
    let q = f.state();
    let event = q["events"]
        .as_object()
        .unwrap()
        .iter()
        .find(|(_, e)| e["task"] == "dev")
        .unwrap()
        .0;
    checked(
        send,
        receive,
        11,
        "apply",
        json!({"task":"dev","state_version":q["tasks"]["dev"]["state_version"],"event":event,
        "output":output,"acceptance":{"goal":"synthetic development completed"},"release_evidence":"bound native turn completed",
        "next_action":{"kind":"complete"}}),
    );
    if staged {
        checked(
            send,
            receive,
            18,
            "lineage",
            json!({"task":"task",
            "state_version":f.state()["tasks"]["task"]["state_version"],
            "kind":"stage","id":"draft","scope":"deliver the developed file","criteria":["goal"]}),
        );
    }
    review_delivery(f, send, receive, output, target, ("review", 12, &f.commit));
    if staged {
        let before = f.state();
        let denied = tool(
            send,
            receive,
            19,
            "finalize",
            json!({"task":"task",
            "state_version":before["tasks"]["task"]["state_version"],"outcome":"success","reason":"stage is not final"}),
        );
        assert_eq!(denied["success"], false);
        assert!(f.state()["tasks"]["task"]["final_outcome"].is_null());
        let id = before["tasks"]["task"]["delivery"].clone();
        let (original_runtime, original_binding) = bound_review_journals(f);
        checked(send, receive, 20, "stage-complete", json!({"delivery":id}));
        assert_selected_original_review(f, &original_runtime, &original_binding);
        await_cleanup_and_ack_notice(f, send, receive, &id);
        let after = f.state();
        assert_eq!(after["tasks"]["task"]["control"], "running");
        assert!(after["tasks"]["task"]["approved"].is_null());
        assert_eq!(
            after["tasks"]["task"]["stage_delivery"]["phase"],
            "complete"
        );
        assert_eq!(before["lifecycle"], after["lifecycle"]);
        assert!(!progress::lineage::current(
            &after,
            &after["tasks"]["review"]
        ));
        let denied = tool(
            send,
            receive,
            21,
            "deliver",
            json!({"adapter":"git","target":target,
            "ref":"refs/heads/accepted","expected":output["id"],"authorization":"deliver-local",
            "state_version":after["tasks"]["task"]["state_version"]}),
        );
        assert_eq!(denied["success"], false);
        review_delivery(
            f,
            send,
            receive,
            output,
            target,
            ("overall-review", 30, output["id"].as_str().unwrap()),
        );
        let final_state = f.state();
        assert!(final_state["tasks"]["task"]["stage"].is_null());
        assert_eq!(final_state["tasks"]["task"]["approved"]["scope_epoch"], 2);
        assert_ne!(
            final_state["tasks"]["review"]["verdict"]["envelope"]["reviewer"],
            final_state["tasks"]["overall-review"]["verdict"]["envelope"]["reviewer"]
        );
    }
}

fn assert_fresh_review_identity(threads: &Mutex<BTreeMap<String, NativeThread>>) {
    let threads = threads.lock().unwrap();
    let retired = threads.get(STAGE_REVIEW).unwrap();
    let fresh = threads.get(OVERALL_REVIEW).unwrap();
    assert!(
        retired.deleted,
        "the stage reviewer must actually be deleted before new acceptance"
    );
    assert!(
        !fresh.deleted,
        "the fresh overall reviewer must survive until the final decision"
    );
    assert_ne!(retired.id, fresh.id);
    assert_ne!(retired.cwd, fresh.cwd);
    let environment = util::read_json(&task_env::environment_state_path("task").unwrap()).unwrap();
    let original = &environment["stage_resources"]["entries"][0]["sessions"][0];
    assert_eq!(original["session"], STAGE_REVIEW);
    assert_eq!(original["phase"], "deleted");
}

fn bound_review_journals(f: &Fixture) -> (PathBuf, J) {
    let state = f.state();
    let root = PathBuf::from(state["waits"]["review"]["runtime"].as_str().unwrap());
    let creation = util::read_state_json(&root, "creation.json").unwrap();
    let turn = util::read_state_json(&root, "turn.json").unwrap();
    let receipt = util::read_state_json(&root, "collection-receipt.json").unwrap();
    let binding = progress::binding(&state["waits"]["review"]).unwrap();
    assert_eq!(
        creation["phase"], "bound",
        "actual native::bind_created terminal phase"
    );
    assert_eq!(
        turn["phase"], "signalled",
        "actual native::collect terminal phase"
    );
    assert_eq!(creation["packet"]["binding"], binding);
    assert_eq!(turn["binding"], binding);
    assert_eq!(receipt["binding"], binding);
    assert_eq!(receipt["runtime"], json!(root));
    assert_eq!(receipt["wait"], "review");
    assert_eq!(
        receipt["owner"],
        json!(lifecycle::task_owner(&state, "review").unwrap().unwrap())
    );
    let original = state["events"]
        .as_object()
        .unwrap()
        .values()
        .find(|event| event["wait"] == "review" && event["binding"] == binding)
        .unwrap();
    assert_eq!(original["acknowledged"], true);
    assert_eq!(original["applied"], true);
    (root, binding)
}

fn assert_selected_original_review(f: &Fixture, root: &Path, binding: &J) {
    let state = f.state();
    let summary = &state["tasks"]["task"]["stage_delivery"]["resource_cleanup"];
    assert_eq!(
        summary["selected"], 1,
        "actual bound/signalled/collected review must be selected: {summary}"
    );
    assert_eq!(summary["retained"], json!([]));
    let environment = util::read_json(&task_env::environment_state_path("task").unwrap()).unwrap();
    let entries = environment["stage_resources"]["entries"]
        .as_array()
        .unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["wait"], "review");
    assert_eq!(entries[0]["root"], json!(root));
    assert_eq!(entries[0]["binding"], *binding);
}

fn await_cleanup_and_ack_notice(
    f: &Fixture,
    send: &mpsc::Sender<J>,
    receive: &mpsc::Receiver<J>,
    stage: &J,
) {
    await_condition(|| {
        f.state()["tasks"]["task"]["stage_delivery"]["resource_cleanup"]["phase"] == "complete"
    });
    let state = f.state();
    let wait = state["waits"]
        .as_object()
        .unwrap()
        .iter()
        .find(|(_, wait)| wait["stage_delivery"] == *stage)
        .unwrap()
        .0
        .clone();
    let event = state["events"]
        .as_object()
        .unwrap()
        .iter()
        .find(|(_, event)| event["wait"] == wait && event["outcome"] == "complete")
        .unwrap()
        .0
        .clone();
    await_condition(|| {
        let state = f.state();
        state["active"].as_str().is_some_and(|active| {
            state["deliveries"][active]["events"]
                .as_array()
                .is_some_and(|events| events.contains(&json!(event)))
        })
    });
    checked(
        send,
        receive,
        22,
        "ack",
        json!({"delivery":f.state()["active"]}),
    );
    let state = f.state();
    assert_eq!(state["events"][&event]["acknowledged"], true);
    assert_ne!(
        state["events"][&event]["applied"], true,
        "host cleanup notice grants no task-apply"
    );
    assert_eq!(
        state["tasks"]["task"]["stage_delivery"]["resource_cleanup"]["completed"],
        1
    );
}

fn review_delivery(
    f: &Fixture,
    send: &mpsc::Sender<J>,
    receive: &mpsc::Receiver<J>,
    output: &J,
    target: &Path,
    review: (&str, u64, &str),
) {
    let (name, id, expected) = review;
    let q = f.state();
    checked(
        send,
        receive,
        id,
        "lineage",
        json!({"task":"task","state_version":q["tasks"]["task"]["state_version"],
        "kind":"candidate","candidate":output["id"],"reason":"developed candidate"}),
    );
    let q = f.state();
    checked(
        send,
        receive,
        id + 1,
        "lineage",
        json!({"task":"task","state_version":q["tasks"]["task"]["state_version"],
        "kind":"activity","activity":name,"role":"acceptance","input":output["id"],"session_epoch":1,
        "result_wait":{"id":name,"kind":"signal","task":name,"epoch":1,"assignment":format!("{name}-a1"), "owner":"reviewer","reason":"independent review","condition":"report","timeout_seconds":600}}),
    );
    checked(
        send,
        receive,
        id + 2,
        "run",
        json!({"wait":name,"instruction":"synthetic independent review"}),
    );
    await_condition(|| {
        f.state()["events"]
            .as_object()
            .unwrap()
            .values()
            .any(|e| e["task"] == name)
    });
    await_condition(|| !f.state()["active"].is_null());
    checked(
        send,
        receive,
        id + 3,
        "ack",
        json!({"delivery":f.state()["active"]}),
    );
    let q = f.state();
    let event = q["events"]
        .as_object()
        .unwrap()
        .iter()
        .find(|(_, e)| e["task"] == name)
        .unwrap()
        .0;
    checked(
        send,
        receive,
        id + 4,
        "apply",
        json!({"task":name,"state_version":q["tasks"][name]["state_version"],"event":event,
        "verdict":q["tasks"][name]["verdict"]["id"],"next_action":{"kind":"delivery","owner":"coordinator","reason":"passed","condition":"predeclared local delivery"}}),
    );
    let q = f.state();
    checked(
        send,
        receive,
        id + 5,
        "deliver",
        json!({"adapter":"git","target":target,"ref":"refs/heads/accepted","expected":expected,
        "authorization":"deliver-local","state_version":q["tasks"]["task"]["state_version"]}),
    );
}

struct ContinuationServer {
    path: PathBuf,
    stopped: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    _directory: TempDir,
}

fn continuation_server(
    run: impl Fn(rpc_server::Peer, Arc<AtomicBool>) + Send + Sync + 'static,
) -> ContinuationServer {
    use std::os::unix::net::UnixListener;
    let directory = common::tempdir().unwrap();
    let path = directory.path().join("rpc.sock");
    let listener = UnixListener::bind(&path).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let stopped = Arc::new(AtomicBool::new(false));
    let (stop, run) = (stopped.clone(), Arc::new(run));
    let thread = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(120);
        let mut peers = vec![];
        while !stop.load(Ordering::SeqCst) {
            assert!(
                Instant::now() < deadline,
                "continuation service exceeded its finite deadline"
            );
            match listener.accept() {
                Ok((stream, _)) => {
                    assert!(
                        peers.len() < 128,
                        "continuation connection inventory exceeds bound"
                    );
                    stream
                        .set_read_timeout(Some(Duration::from_secs(3)))
                        .unwrap();
                    stream
                        .set_write_timeout(Some(Duration::from_secs(3)))
                        .unwrap();
                    let (run, stop) = (run.clone(), stop.clone());
                    peers.push(std::thread::spawn(move || {
                        run(rpc_server::Peer { stream }, stop)
                    }));
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(2))
                }
                Err(error) => panic!("continuation fixture accept: {error}"),
            }
        }
        for peer in peers {
            peer.join().unwrap();
        }
    });
    ContinuationServer {
        path,
        stopped,
        thread: Some(thread),
        _directory: directory,
    }
}

impl ContinuationServer {
    fn finish(mut self) {
        self.shutdown();
    }
    fn shutdown(&mut self) {
        self.stopped.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let joined = thread.join();
            if !std::thread::panicking() {
                joined.unwrap();
            }
        }
    }
}

impl Drop for ContinuationServer {
    fn drop(&mut self) {
        self.shutdown();
    }
}

pub(super) fn parallel(
    count: usize,
    run: impl Fn(rpc_server::Peer) + Send + Sync + 'static,
) -> rpc_server::Server {
    let run = std::sync::Arc::new(run);
    let mut children = Vec::new();
    let mut accepted = 0;
    rpc_server::Server::start_connections_with_timeout(
        count,
        Duration::from_secs(90),
        move |peer| {
            let run = run.clone();
            children.push(std::thread::spawn(move || run(peer)));
            accepted += 1;
            if accepted == count {
                for child in children.drain(..) {
                    child.join().unwrap();
                }
            }
        },
    )
}
