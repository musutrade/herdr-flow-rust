//! Synthetic native protocol over real isolated queue/instance ownership.
use super::*;
#[test]
#[ignore = "subprocess-only Codex protocol fixture; no model"]
fn rpc_child() {
    use std::os::unix::net::UnixListener;
    // Set this after the namespace bootstrap/exec so credential transitions
    // cannot clear it. The fixture must not outlive the launcher held by its
    // FixtureChild guard, including when a test unwinds before finalization.
    // SAFETY: getppid/prctl take scalar arguments and do not borrow memory.
    unsafe {
        let parent = libc::getppid();
        assert_eq!(
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0),
            0
        );
        assert_eq!(
            libc::getppid(),
            parent,
            "fixture launcher exited during setup"
        );
    }
    let root = PathBuf::from(std::env::var_os("HERDR_ISOLATED_RPC_FIXTURE").unwrap());
    let config: J =
        serde_json::from_slice(&fs::read(root.join("tmp/fixture.json")).unwrap()).unwrap();
    let listener = UnixListener::bind(root.join("server.sock")).unwrap();
    fs::set_permissions(root.join("server.sock"), fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(root.join("ready"), std::process::id().to_string()).unwrap();
    let session = if root.ends_with("coordinator") {
        "22222222-2222-2222-2222-222222222222"
    } else {
        "11111111-1111-1111-1111-111111111111"
    };
    let mut turns = json!([]);
    for stream in listener.incoming() {
        let mut peer = rpc_server::Peer {
            stream: stream.unwrap(),
        };
        peer.initialize();
        loop {
            use std::os::fd::AsRawFd;
            let mut byte = [0u8];
            // SAFETY: the stream owns its fd and byte is a live output buffer.
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
            let (opcode, bytes) = peer.frame();
            if opcode == 8 {
                break;
            }
            let request: J = serde_json::from_slice(&bytes).unwrap();
            let thread = json!({"id":session,"cwd":config["cwd"],"modelProvider":"openai","model":"fixture","turns":turns,
                "status":{"type":"idle"},"canAcceptDirectInput":true});
            if request["method"] == "thread/turns/list" {
                peer.json(&json!({"id":request["id"],"error":{"code":-32601,"message":"use native events"}}));
                continue;
            }
            let result = match request["method"].as_str().unwrap() {
                "thread/start" | "thread/read" => {
                    json!({"thread":thread,"model":"fixture","modelProvider":"openai"})
                }
                "turn/start" => {
                    turns = json!([{"id":"fixture-turn","status":"completed","items":[{"type":"agentMessage","text":"synthetic review"}]}]);
                    json!({"turn":{"id":"fixture-turn"}})
                }
                method => panic!("unexpected method {method}"),
            };
            peer.json(&json!({"id":request["id"],"result":result}));
            if request["method"] == "turn/start" {
                peer.json(&json!({"method":"turn/completed","params":{"threadId":session,"turn":turns[0]}}));
            }
        }
    }
}

pub(super) fn linked(f: &Fixture) -> task_env::TaskEnvironment {
    linked_goal(f, "fixture")
}

pub(super) fn linked_goal(f: &Fixture, goal: &str) -> task_env::TaskEnvironment {
    linked_delivery(f, goal, None)
}

pub(super) fn linked_delivery(
    f: &Fixture,
    goal: &str,
    target: Option<&Path>,
) -> task_env::TaskEnvironment {
    let mut q = f.state();
    q["tasks"].as_object_mut().unwrap().remove("task");
    let mut plan = json!({"kind":"plan","task":"task","goal":goal,
        "basis":{"original":[goal],"changes":[],"specifications":[],"endpoint":"local","confirmation":"fixture"},
        "authorization":[{"id":"develop","action":"development","source":"fixture"},
            {"id":"verify","action":"review","source":"fixture"}]});
    if target.is_some() {
        plan["authorization"].as_array_mut().unwrap().push(json!({"id":"deliver-local","action":"delivery","source":"isolated acceptance harness: only its new local output repository"}));
    }
    progress::lineage::action(&mut q, &plan, util::now()).unwrap();
    for mut spec in [
        json!({"kind":"artifact","artifact":{"id":f.commit,"adapter":"git","location":f.temp.path().join("repo")}}),
        json!({"kind":"candidate","candidate":f.commit}),
    ] {
        spec["task"] = json!("task");
        spec["reason"] = json!("explicit fixture candidate");
        spec["state_version"] = q["tasks"]["task"]["state_version"].clone();
        progress::lineage::action(&mut q, &spec, util::now()).unwrap();
    }
    queue::save(&f.queue, &mut q).unwrap();
    let mut create = f.create_spec();
    if let Some(target) = target {
        create["environment"]["delivery_targets"] =
            json!([{"path":target,"target_type":"git","description":"isolated acceptance output"}]);
    }
    f.call(&create).unwrap();
    let env = task_env::restore_environment("task").unwrap();
    let mut q = f.state();
    let spec = json!({"kind":"activity","task":"task","state_version":q["tasks"]["task"]["state_version"],
        "activity":"dev","role":"development","input":f.commit,"session_epoch":1,
        "result_wait":{"id":"dev","kind":"signal","task":"dev","epoch":1,"assignment":"dev-a1",
            "owner":"developer","reason":"fixture","condition":"report","timeout_seconds":600}});
    progress::lineage::action(&mut q, &spec, util::now()).unwrap();
    queue::save(&f.queue, &mut q).unwrap();
    env
}

fn runtime(env: &mut task_env::TaskEnvironment) -> (PathBuf, FixtureChild) {
    let cwd = env.workspace_root.join("checkout");
    runtime_at(env, "dev-a1", &cwd, None)
}

pub(super) fn runtime_at(
    env: &mut task_env::TaskEnvironment,
    assignment: &str,
    cwd: &Path,
    review: Option<&str>,
) -> (PathBuf, FixtureChild) {
    let root = task_runtime::prepare(env, assignment).unwrap();
    util::atomic_write_json(&root.join("tmp/fixture.json"), &json!({"cwd":cwd})).unwrap();
    let binary = root.join("fixture-server");
    fs::write(&binary, format!("#!/bin/sh\nexport HERDR_ISOLATED_RPC_FIXTURE='{}'\nexec '{}' --exact isolated_runtime::rpc_child --ignored --nocapture\n",
        root.display(), std::env::current_exe().unwrap().display())).unwrap();
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
    let mut spec = json!({"adapter":"codex","configuration":"isolated-fixture","binary":binary,"cwd":cwd,"driver_binary":env!("CARGO_BIN_EXE_herdr-flow"),
        "model":{"providerID":"openai","id":"fixture"}});
    if let Some(key) = review {
        spec["review_snapshot"] = json!(key);
    }
    let mut child = FixtureChild(task_runtime::launch(env, &root, &spec).unwrap());
    let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !root.join("ready").exists() {
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "fixture exited: {}",
            fs::read_to_string(root.join("server.log")).unwrap_or_default()
        );
        assert!(
            std::time::Instant::now() < until,
            "fixture startup timed out"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    (root, child)
}

fn create_spec(f: &Fixture) -> J {
    json!({"queue_dir":f.queue,"wait":"dev","instruction":"fixture only","evidence":[]})
}

#[test]
fn fixture_server_exits_when_its_namespace_launcher_is_killed() {
    let f = Fixture::new();
    let mut env = linked(&f);
    let (root, mut child) = runtime(&mut env);
    let pid: i32 = fs::read_to_string(root.join("ready"))
        .unwrap()
        .parse()
        .unwrap();
    let identity = task_env::process_start_time(pid).unwrap();
    assert_ne!(pid, child.0.id() as i32);
    child.0.kill().unwrap();
    child.0.wait().unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while task_env::process_start_time(pid) == Some(identity) {
        let stat = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
        if stat
            .rsplit_once(") ")
            .is_some_and(|(_, fields)| fields.starts_with('Z'))
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "fixture server {pid} survived launcher exit"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    drop(env);
    upstream_runtime::finalize_with_retry(&f);
    let report = queue::finalization::drive(&f.queue).unwrap();
    assert_eq!(report.pending, 0, "{report:?}");
    assert!(!root.exists());
}

#[test]
fn bound_control_queue_outside_roots_supports_create_and_recover() {
    let f = Fixture::new();
    let mut env = linked(&f);
    assert!(task_env::validate_path(&env, &f.queue).is_err());
    let (root, _child) = runtime(&mut env);
    let state = task_runtime::native::create(&env, &root, &create_spec(&f)).unwrap();
    let before = f.state();
    let recovered = task_runtime::native::recover(&env, &root).unwrap();
    assert_eq!(recovered["session"], state["session"]);
    assert_eq!(
        f.state()["waits"]["dev"]["producer"],
        before["waits"]["dev"]["producer"]
    );
    assert!(task_env::validate_path(&env, &f.queue).is_err());
}

#[test]
fn control_queue_rejects_foreign_paths_before_creating_authority() {
    let f = Fixture::new();
    let mut env = linked(&f);
    let (root, _child) = runtime(&mut env);
    for path in [
        env.temp_root.join("foreign"),
        f.queue.join("child"),
        f.temp.path().join("foreign"),
    ] {
        let mut spec = create_spec(&f);
        spec["queue_dir"] = json!(path);
        let before = f.state();
        assert!(task_runtime::native::create(&env, &root, &spec).is_err());
        assert!(!path.exists(), "foreign queue access created a directory");
        assert!(!root.join("creation.json").exists());
        assert_eq!(f.state(), before);
    }
}

#[test]
fn control_queue_recovery_rejects_foreign_journal_and_alias() {
    let f = Fixture::new();
    let mut env = linked(&f);
    let (root, _child) = runtime(&mut env);
    let alias = f.temp.path().join("queue-alias");
    std::os::unix::fs::symlink(&f.queue, &alias).unwrap();
    let mut spec = create_spec(&f);
    spec["queue_dir"] = json!(alias);
    assert!(task_runtime::native::create(&env, &root, &spec).is_err());
    assert!(!root.join("creation.json").exists());
    task_runtime::native::create(&env, &root, &create_spec(&f)).unwrap();
    let before = f.state();
    let original = util::read_json(&root.join("creation.json")).unwrap();
    let state_before = util::read_json(&root.join("state.json")).unwrap();
    let foreign = env.temp_root.join("foreign");
    for path in [&foreign, &alias] {
        let mut journal = original.clone();
        journal["queue_dir"] = json!(path);
        util::atomic_write_json(&root.join("creation.json"), &journal).unwrap();
        assert!(task_runtime::native::recover(&env, &root).is_err());
        assert_eq!(
            util::read_json(&root.join("creation.json")).unwrap(),
            journal
        );
        assert_eq!(
            util::read_json(&root.join("state.json")).unwrap(),
            state_before
        );
        assert_eq!(f.state(), before);
        assert!(!foreign.exists());
    }
    util::atomic_write_json(&root.join("creation.json"), &original).unwrap();
    task_runtime::native::recover(&env, &root).unwrap();
}

#[test]
fn control_queue_never_grants_old_or_closing_owners_access() {
    let f = Fixture::new();
    let mut env = linked(&f);
    let (root, _child) = runtime(&mut env);
    let old = env.creation.as_ref().unwrap().binding.clone();
    env.creation.as_mut().unwrap().binding.epoch += 1;
    assert!(task_runtime::native::create(&env, &root, &create_spec(&f)).is_err());
    env.creation.as_mut().unwrap().binding = old;
    let saved_instance = env.instance_id.clone();
    env.instance_id = "foreign-instance".into();
    assert!(task_runtime::native::create(&env, &root, &create_spec(&f)).is_err());
    env.instance_id = saved_instance;
    task_runtime::native::create(&env, &root, &create_spec(&f)).unwrap();
    drop(env);
    f.finalize("failure").unwrap();
    let env = task_env::restore_environment("task").unwrap();
    assert!(task_runtime::native::create(&env, &root, &create_spec(&f)).is_err());
    assert!(task_runtime::native::recover(&env, &root).is_err());
}

#[test]
fn replaced_bound_queue_is_never_adopted_or_recreated() {
    let f = Fixture::new();
    let mut env = linked(&f);
    let (root, _child) = runtime(&mut env);
    task_runtime::native::create(&env, &root, &create_spec(&f)).unwrap();
    let broker_pid = util::read_state_json(&root, "broker-owner.json").unwrap()["pid"]
        .as_i64()
        .unwrap() as i32;
    let original = f.state();
    let moved = f.temp.path().join("original-queue");
    fs::rename(&f.queue, &moved).unwrap();
    fs::create_dir(&f.queue).unwrap();
    fs::write(f.queue.join("sentinel"), "foreign").unwrap();
    assert!(task_runtime::native::create(&env, &root, &create_spec(&f)).is_err());
    assert!(task_runtime::native::recover(&env, &root).is_err());
    assert_eq!(
        fs::read_to_string(f.queue.join("sentinel")).unwrap(),
        "foreign"
    );
    assert!(!f.queue.join("queue.lock").exists());
    assert!(!f.queue.join("queue.json").exists());
    // Losing the bound queue stops the Codex broker. Reap our child before
    // restoring the queue and restarting it; a zombie still owns its PID.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        // SAFETY: this is our recorded broker child and no status pointer is used.
        let reaped = unsafe { libc::waitpid(broker_pid, std::ptr::null_mut(), libc::WNOHANG) };
        if reaped == broker_pid {
            break;
        }
        assert_eq!(reaped, 0, "broker child could not be reaped");
        assert!(std::time::Instant::now() < deadline, "broker did not stop");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    fs::remove_file(f.queue.join("sentinel")).unwrap();
    fs::remove_dir(&f.queue).unwrap();
    fs::rename(&moved, &f.queue).unwrap();
    assert_eq!(f.state(), original);
    task_runtime::start_broker(&mut env, &root, &json!({})).unwrap_or_else(|error| {
        panic!(
            "{error}: {}",
            fs::read_to_string(root.join("broker.log")).unwrap_or_default()
        )
    });
    task_runtime::native::recover(&env, &root).unwrap();
}
