//! The production attach/broker/finalizer against a shared protocol service.
use super::*;
use std::sync::mpsc;

const OWNED: &str = "22222222-2222-2222-2222-222222222222";
const FOREIGN: &str = "33333333-3333-3333-3333-333333333333";

#[test]
fn upstream_thread_cleanup_preserves_service_config_and_other_sessions() {
    for lost_reply in [false, true] {
        exercise(lost_reply);
    }
}

fn exercise(lost_reply: bool) {
    let f = Fixture::new();
    let mut env = f.ready();
    let root = task_runtime::prepare(&env, "shared").unwrap();
    let cwd = env.workspace_root.join("checkout");
    let (release, wait) = mpsc::channel();
    let server_cwd = cwd.clone();
    let mut index = 0;
    let server = rpc_server::Server::start_connections(
        if lost_reply { 4 } else { 3 },
        move |mut peer| {
            peer.initialize();
            let request = peer.request();
            if index == 0 {
                assert_eq!(request["method"], "thread/start");
                peer.json(&json!({"id":request["id"],"result":{
                "thread":{"id":OWNED,"cwd":server_cwd,"modelProvider":"openai","turns":[]},
                "model":"fixture","modelProvider":"openai"}}));
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
                while wait.try_recv().is_err() {
                    assert!(std::time::Instant::now() < deadline);
                    if !readable(&peer) {
                        continue;
                    }
                    let request = peer.request();
                    let result = match request["method"].as_str().unwrap() {
                        "thread/read" => json!({"thread":{"id":OWNED,"cwd":server_cwd,
                            "modelProvider":"openai","status":{"type":"idle"},"canAcceptDirectInput":true}}),
                        "turn/start" => json!({"turn":{"id":"fixture-turn"}}),
                        method => panic!("unexpected {method}"),
                    };
                    assert_eq!(request["params"]["threadId"], OWNED);
                    peer.json(&json!({"id":request["id"],"result":result}));
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
    let adapter = herdr_flow::herdr::receiver_factory(&active).unwrap();
    assert_eq!(adapter.agent().unwrap()["interactive_ready"], true);
    assert_eq!(
        adapter.prompt("synthetic protocol only").unwrap(),
        "fixture-turn"
    );
    assert!(adapter.call(&["notify".into()]).is_err());
    let mut wrong = active;
    wrong["session"] = json!(FOREIGN);
    assert!(herdr_flow::herdr::receiver_factory(&wrong).is_err());

    assert!(fs::read_dir(root.join("c")).unwrap().next().is_none());
    release.send(()).unwrap();
    drop(env);
    finalize_with_retry(&f);
    let report = queue::finalization::drive(&f.queue).unwrap();
    if lost_reply {
        assert_eq!(report.pending, 1, "{report:?}");
        assert!(root.exists());
        assert!(task_env::is_registered("task").unwrap());
        assert_eq!(queue::finalization::drive(&f.queue).unwrap().pending, 0);
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
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        match finalize_registered(&f.queue) {
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
    let report = queue::finalization::drive(&queue_root).unwrap();
    assert_eq!(report.pending, 0, "{report:?}");
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
    let _lock = queue::locked(root, false)?;
    let state = queue::load_queue(root)?;
    let owner = lifecycle::task_owner(&state, "task")?.unwrap();
    let receiver = util::read_state_json(
        Path::new(state["guard_dir"].as_str().unwrap()),
        "state.json",
    )?;
    let spec = json!({"task":"task","queue_id":owner.queue_id,"epoch":owner.epoch,
        "instance_id":owner.instance_id,"state_version":state["tasks"]["task"]["state_version"],
        "outcome":"failure","reason":"explicit administrative teardown of acceptance resources; no delivery claim"});
    queue::task_action(
        root,
        "task-finalize",
        &spec,
        receiver["session"].as_str(),
        &|_| panic!("no model"),
    )
}
