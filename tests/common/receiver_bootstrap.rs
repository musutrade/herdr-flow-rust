//! Synthetic native protocol through the real task-owned broker; no models.
use super::*;
use herdr_flow::{guard, herdr, opencode::session, runtime, util};
use task_runtime::native::receiver;

fn start(env: &mut TaskEnvironment, control: &J) -> std::path::PathBuf {
    let root = task_runtime::prepare(env, "receiver").unwrap();
    let cwd = env.workspace_root.clone();
    let binary = fixture_binary(&root, &cwd, 0);
    util::atomic_write_json(&root.join("receiver-fixture.json"), control).unwrap();
    // Synthetic server, deliberately independent of host namespace support.
    let spec = json!({"adapter":"opencode","configuration":"isolated-fixture","binary":binary,"cwd":cwd,"port":1,
        "model":{"providerID":"openai","id":"fixture"}});
    let mut child = task_runtime::launch(env, &root, &spec).unwrap();
    fixture_ready(&root, &mut child);
    let mut record = util::read_json(&root.join("runtime.json")).unwrap();
    record["adapter"] = json!("codex");
    util::atomic_write_json(&root.join("runtime.json"), &record).unwrap();
    task_runtime::start_broker(
        env,
        &root,
        &json!({"driver_binary":env!("CARGO_BIN_EXE_herdr-flow")}),
    )
    .unwrap();
    root
}

fn spec(root: &Path) -> J {
    json!({"guard_dir":root.join("receiver"),"task":"parent","role":"coordinator"})
}

fn calls(root: &Path) -> J {
    util::read_json(&root.join("receiver-calls.json")).unwrap()
}

fn synthetic_healthy_watcher(root: &Path) -> util::FileLock {
    let pid = std::process::id();
    util::atomic_write_json(
        &root.join("watch.json"),
        &json!({
            "pid":pid,"start_ticks":util::process_start_ticks(pid).unwrap(),
            "heartbeat_at":util::now(),"max_age":600,"status":"running"
        }),
    )
    .unwrap();
    let lock = util::FileLock::acquire(&root.join("watch.lock"), false)
        .unwrap()
        .unwrap();
    assert_eq!(herdr_flow::watch_service::health(root)["healthy"], true);
    lock
}

#[test]
fn bootstrap_creates_once_without_worker_or_queue_mutation() {
    let _lock = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let temp = temp();
    let mut env = environment(temp.path());
    let q = env.temp_root.join("queue");
    queue::init(&q, None, 30).unwrap();
    let before = fs::read(q.join("queue.json")).unwrap();
    let root = start(&mut env, &json!({}));
    let spec = spec(&root);
    let state = receiver::bootstrap(&env, &root, &spec).unwrap();
    assert_eq!(state["phase"], "bootstrap");
    assert_eq!(state["instance"], env.instance_id);
    assert_eq!(state["auto_rotate"], false);
    assert_eq!(state["runtime"], json!(root));
    assert_eq!(state["task"], "parent");
    assert_eq!(fs::read(q.join("queue.json")).unwrap(), before);
    assert!(!root.join("creation.json").exists());
    assert!(!root.join("state.json").exists());
    assert!(!root.join("receiver/watch.json").exists());
    assert_eq!(
        util::read_json(&root.join("receiver.json")).unwrap()["phase"],
        "bound"
    );
    assert!(receiver::bootstrap(&env, &root, &spec).is_err());
    assert!(task_runtime::native::create(&env, &root, &json!({"queue_dir":q})).is_err());
    let calls = calls(&root);
    assert_eq!(calls.as_array().unwrap().len(), 1);
    assert_eq!(calls[0]["method"], "thread/start");
    assert_eq!(calls[0]["params"]["approvalPolicy"], "untrusted");
    assert_eq!(calls[0]["params"]["sandbox"], "workspace-write");
    assert_eq!(calls[0]["params"]["ephemeral"], false);
    assert_eq!(
        calls[0]["params"]["baseInstructions"],
        "Coordinate only the explicitly assigned task. Report evidence and blockers."
    );
}

#[test]
fn bootstrap_retains_uncertainty_for_transport_and_identity_failures() {
    let _lock = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    for failure in [
        "disconnect",
        "rpc-error",
        "id",
        "cwd",
        "model",
        "provider",
        "thread-provider",
        "turns",
    ] {
        let temp = temp();
        let mut env = environment(temp.path());
        let mut response = json!({"thread":{"id":"11111111-1111-1111-1111-111111111111",
            "cwd":env.workspace_root,"modelProvider":"openai","turns":[]},
            "model":"fixture","modelProvider":"openai"});
        match failure {
            "id" => response["thread"]["id"] = json!(""),
            "cwd" => response["thread"]["cwd"] = json!("/wrong"),
            "model" => response["model"] = json!("wrong"),
            "provider" => response["modelProvider"] = json!("wrong"),
            "thread-provider" => response["thread"]["modelProvider"] = json!("wrong"),
            "turns" => response["thread"]["turns"] = json!([{"id":"previous"}]),
            _ => (),
        }
        let root = start(&mut env, &json!({"mode":failure,"response":response}));
        let spec = spec(&root);
        assert!(
            receiver::bootstrap(&env, &root, &spec).is_err(),
            "{failure}"
        );
        assert_eq!(
            util::read_json(&root.join("receiver.json")).unwrap()["phase"],
            "uncertain"
        );
        assert!(!root.join("receiver/state.json").exists());
        assert!(receiver::bootstrap(&env, &root, &spec).is_err());
        assert_eq!(calls(&root).as_array().unwrap().len(), 1);
    }
}

#[test]
fn bootstrap_rejects_invalid_preconditions_without_native_request() {
    let _lock = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let temp = temp();
    let mut env = environment(temp.path());
    let root = start(&mut env, &json!({}));
    for path in [
        env.workspace_root.join("guard"),
        env.temp_root.join("guard"),
        root.clone(),
    ] {
        assert!(receiver::bootstrap(&env, &root, &json!({"guard_dir":path})).is_err());
    }
    let original = util::read_json(&root.join("runtime.json")).unwrap();
    for (field, value) in [
        ("instance", json!("other")),
        ("adapter", json!("opencode")),
        ("phase", json!("starting")),
        ("pid", json!(0)),
        ("start_time", json!(0)),
        ("approval_policy", json!("never")),
        ("model", json!({})),
    ] {
        let mut changed = original.clone();
        changed[field] = value;
        util::atomic_write_json(&root.join("runtime.json"), &changed).unwrap();
        assert!(
            receiver::bootstrap(&env, &root, &spec(&root)).is_err(),
            "{field}"
        );
    }
    util::atomic_write_json(&root.join("runtime.json"), &original).unwrap();
    // Existing worker attempt reserves the runtime even when the symlink dangles.
    std::os::unix::fs::symlink("missing", root.join("creation.json")).unwrap();
    assert!(receiver::bootstrap(&env, &root, &spec(&root)).is_err());
    fs::remove_file(root.join("creation.json")).unwrap();
    fs::write(root.join("receiver/state.json"), "existing").unwrap();
    assert!(receiver::bootstrap(&env, &root, &spec(&root)).is_err());
    assert!(!root.join("receiver.json").exists());
    assert!(!root.join("receiver-calls.json").exists());
}

#[test]
fn bootstrap_never_borrows_ambient_session_or_becomes_ready() {
    let _lock = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let temp = temp();
    let mut env = environment(temp.path());
    let root = start(&mut env, &json!({}));
    let mut state = receiver::bootstrap(&env, &root, &spec(&root)).unwrap();
    let g = root.join("receiver");
    // Control state stays outside the model runtime's writable roots. These
    // fixtures test protocol refusal, not integrity against model-side writes.
    let q = env.temp_root.join("queue");
    assert!(!herdr_flow::task_env::paths_overlap(&q, &root));
    assert!(!herdr_flow::task_env::paths_overlap(
        &q,
        &env.workspace_root
    ));
    let mut queue = queue::init(&q, Some(&g), 30).unwrap();
    assert!(guard::watch(&g, None, true)
        .unwrap_err()
        .message
        .contains("not activated"));
    assert!(!g.join("watch.json").exists());
    let ambient = std::env::var_os("CODEX_THREAD_ID");
    std::env::set_var("CODEX_THREAD_ID", "unrelated-ambient-session");
    let session_result = session::receiver_session(&q);
    if let Some(value) = ambient {
        std::env::set_var("CODEX_THREAD_ID", value);
    } else {
        std::env::remove_var("CODEX_THREAD_ID");
    }
    assert!(session_result
        .unwrap_err()
        .message
        .contains("not activated"));
    let _guard_watcher = synthetic_healthy_watcher(&g);
    let _queue_watcher = synthetic_healthy_watcher(&q);
    for phase in ["bootstrap", "monitoring"] {
        state["phase"] = json!(phase);
        util::atomic_write_json(&g.join("state.json"), &state).unwrap();
        assert!(guard::validate_watch(&g)
            .unwrap_err()
            .message
            .contains("not activated"));
        assert!(herdr::receiver_factory(&state).is_err());
        assert!(queue::enable(&q, &mut queue, &herdr::receiver_factory).is_err());
        let readiness =
            runtime::readiness(&q, state["session"].as_str(), &herdr::receiver_factory).unwrap();
        assert_eq!(readiness["active_wakeup"], false);
    }
    // Even forged monitoring/enabled/healthy metadata cannot reach delivery.
    queue["notify"] = json!(true);
    queue["events"] = json!({"event":{"id":"event","wait":"test","kind":"signal"}});
    queue::save(&q, &mut queue).unwrap();
    let readiness =
        runtime::readiness(&q, state["session"].as_str(), &herdr::receiver_factory).unwrap();
    assert_eq!(readiness["active_wakeup"], false);
    assert!(readiness["gaps"].as_array().unwrap().iter().any(|gap| gap
        .as_str()
        .is_some_and(|text| text.contains("not activated"))));
    let observed = queue::tick(&q, &herdr::receiver_factory, None).unwrap();
    assert_eq!(observed["delivery_status"], "needs_attention");
    assert!(observed["delivery_error"]
        .as_str()
        .unwrap()
        .contains("not activated"));
    assert!(observed["active"].is_null());
    assert_eq!(calls(&root).as_array().unwrap().len(), 1);
    assert!(queue::load_queue(&q).unwrap()["deliveries"]
        .as_object()
        .unwrap()
        .is_empty());
}

#[test]
fn bootstrap_scope_binds_native_identity_without_changing_legacy_scopes() {
    let base = json!({"task":"t","role":"coordinator","pane":"p","terminal":"term","socket":"sock","cwd":"/work"});
    assert_eq!(guard::scope(&base), base);
    let mut state = base.clone();
    state["adapter"] = json!("codex");
    assert_eq!(guard::scope(&state), base);
    state["receiver_kind"] = json!("task-runtime");
    state["runtime"] = json!("/runtime");
    state["instance"] = json!("instance");
    state["session"] = json!("session");
    state["model"] = json!({"providerID":"openai","id":"fixture"});
    let scope = guard::scope(&state);
    for key in ["runtime", "instance", "session", "model", "cwd"] {
        let mut changed = state.clone();
        changed[key] = json!("changed");
        assert_ne!(guard::scope(&changed), scope, "{key}");
    }
    state["adapter"] = json!("opencode");
    state["endpoint"] = json!("http://127.0.0.1:1");
    state["server_version"] = json!("1.19.4");
    let scope = guard::scope(&state);
    assert!(scope.get("codex_runtime").is_none());
    assert_eq!(scope["opencode"]["session"], "session");
}

#[test]
fn bootstrap_guard_write_failure_retains_created_identity_without_replay() {
    let _lock = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let temp = temp();
    let mut env = environment(temp.path());
    let root = start(&mut env, &json!({"mode":"state-write-error"}));
    assert!(receiver::bootstrap(&env, &root, &spec(&root)).is_err());
    let journal = util::read_json(&root.join("receiver.json")).unwrap();
    assert_eq!(journal["phase"], "uncertain");
    assert_eq!(
        journal["state"]["session"],
        "11111111-1111-1111-1111-111111111111"
    );
    assert!(util::read_state_json(&root.join("receiver"), "state.json").is_err());
    assert!(receiver::bootstrap(&env, &root, &spec(&root)).is_err());
    assert_eq!(calls(&root).as_array().unwrap().len(), 1);
}

#[test]
fn bootstrap_crash_marker_and_absent_broker_cannot_create_threads() {
    let _lock = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let temp = temp();
    let mut env = environment(temp.path());
    let root = start(&mut env, &json!({}));
    util::atomic_write_json(&root.join("receiver.json"), &json!({"phase":"creating"})).unwrap();
    assert!(receiver::bootstrap(&env, &root, &spec(&root)).is_err());
    assert!(!root.join("receiver-calls.json").exists());
    // Separate fresh runtime: fail local broker preflight before reserving intent.
    let other = task_runtime::prepare(&env, "no-broker").unwrap();
    let mut record = util::read_json(&root.join("runtime.json")).unwrap();
    record["root"] = json!(other);
    util::atomic_write_json(&other.join("runtime.json"), &record).unwrap();
    assert!(receiver::bootstrap(&env, &other, &spec(&other))
        .unwrap_err()
        .message
        .contains("broker is unavailable"));
    assert!(!other.join("receiver.json").exists());
    assert!(!other.join("receiver/state.json").exists());
}

#[test]
fn bootstrap_rejects_guard_aliases_and_cwd_overlap_before_request() {
    let _lock = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let temp = temp();
    let mut env = environment(temp.path());
    let root = start(&mut env, &json!({}));
    common::create_dir(root.join("actual")).unwrap();
    std::os::unix::fs::symlink(root.join("actual"), root.join("alias")).unwrap();
    for guard in [root.join("alias/guard"), root.join("actual/../guard")] {
        let error = receiver::bootstrap(&env, &root, &json!({"guard_dir":guard})).unwrap_err();
        assert!(error.message.contains("canonical directory"), "{error}");
    }
    let original = util::read_json(&root.join("runtime.json")).unwrap();
    // Equal, ancestor and descendant cwd all overlap an otherwise valid guard.
    for cwd in [
        root.join("receiver"),
        root.clone(),
        root.join("receiver/work"),
    ] {
        let mut record = original.clone();
        record["cwd"] = json!(cwd);
        util::atomic_write_json(&root.join("runtime.json"), &record).unwrap();
        let error = receiver::bootstrap(&env, &root, &spec(&root)).unwrap_err();
        assert!(error.message.contains("outside cwd"), "{error}");
    }
    util::atomic_write_json(&root.join("runtime.json"), &original).unwrap();
    assert!(!root.join("receiver.json").exists());
    assert!(!root.join("receiver-calls.json").exists());
}
