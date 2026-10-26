//! Explicit real-model review of a fixed local snapshot. This is a caller-driven
//! acceptance scenario, not a claim of autonomous notification/ACK/publication.
use super::*;

#[test]
#[ignore = "explicit existing service/model; one real review turn in a new private queue"]
fn live_upstream_fixed_snapshot_review() {
    let socket = std::env::var("HERDR_UPSTREAM_SOCKET").expect("existing socket required");
    let model: J = serde_json::from_str(&std::env::var("HERDR_UPSTREAM_MODEL").unwrap()).unwrap();
    let f = std::mem::ManuallyDrop::new(Fixture::new());
    eprintln!("real review evidence root: {}", f.temp.path().display());
    let goal = "The tracked file named file must contain exactly review candidate followed by one newline.";
    let mut env = isolated_runtime::linked_goal(&f, goal);
    isolated_review::review_activity(&f, &env);
    let coord = task_runtime::prepare(&env, "coordinator").unwrap();
    let cwd = env.workspace_root.join("checkout");
    let mut coord_broker = attach(&mut env, &coord, &socket, &model, &cwd, None);
    let guard = coord.join("guard");
    let coordinator = task_runtime::native::receiver::bootstrap(
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
    {
        let _lock = queue::locked(&f.queue, false).unwrap();
        let mut q = f.state();
        let state = util::read_state_json(&guard, "state.json").unwrap();
        q["guard_dir"] = json!(guard);
        q["receiver"] = guard::scope(&state);
        q["notify"] = json!(true);
        queue::save(&f.queue, &mut q).unwrap();
    }
    let log = fs::File::create(f.temp.path().join("queue-watch.log")).unwrap();
    let mut watch = FixtureChild(
        Command::new(env!("CARGO_BIN_EXE_herdr-flow"))
            .args(["queue", "watch", "--state-dir"])
            .arg(&f.queue)
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap(),
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while herdr_flow::watch_service::health(&queue::artifacts(&f.queue).unwrap())["healthy"] != true
    {
        assert!(watch.0.try_wait().unwrap().is_none());
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let readiness = herdr_flow::runtime::readiness(
        &f.queue,
        coordinator["session"].as_str(),
        &herdr_flow::herdr::receiver_factory,
    )
    .unwrap();
    util::atomic_write_json(&f.temp.path().join("readiness.json"), &readiness).unwrap();
    assert_eq!(readiness["active_wakeup"], true, "{readiness}");
    let root = task_runtime::prepare(&env, "review-a1").unwrap();
    let snapshot = task_runtime::review::prepare(
        &mut env,
        &root,
        &json!({"queue_dir":f.queue,"wait":"review"}),
    )
    .unwrap();
    let review_cwd = PathBuf::from(snapshot["cwd"].as_str().unwrap());
    let mut review_broker = attach(
        &mut env,
        &root,
        &socket,
        &model,
        &review_cwd,
        Some("review-a1"),
    );
    let state = task_runtime::native::create(&env, &root, &json!({"queue_dir":f.queue,"wait":"review",
        "instruction":"Inspect the tracked file in this fixed snapshot against the supplied requirement. Write .herdr-verdict.json using acceptance_protocol.shape with one check for criterion goal. Use the exact supplied original requirement as source. Do not alter tracked files. No network, git commit, login or configuration changes. Finish after writing the verdict.","evidence":[]})).unwrap();
    util::atomic_write_json(
        &f.temp.path().join("sessions.json"),
        &json!({"coordinator":coordinator["session"],"reviewer":state["session"]}),
    )
    .unwrap();
    task_runtime::native::dispatch(&env, &root).unwrap();
    // This acceptance explicitly uses caller-driven result collection. Disable
    // notifications in this new queue before results; never fake a delivery ACK.
    {
        let _lock = queue::locked(&f.queue, false).unwrap();
        let mut q = f.state();
        q["notify"] = json!(false);
        queue::save(&f.queue, &mut q).unwrap();
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(180);
    let result = loop {
        let result = task_runtime::native::collect(&env, &root).unwrap();
        if result["phase"] == "signalled" {
            break result;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "real review timeout: {result}"
        );
        std::thread::sleep(std::time::Duration::from_secs(1));
    };
    let verdict = util::read_json(&review_cwd.join(".herdr-verdict.json")).unwrap();
    util::atomic_write_json(
        &f.temp.path().join("review-result.json"),
        &json!({"turn":result,"verdict":verdict,"snapshot":snapshot,
        "notification_delivery":"not exercised; caller-driven collection","model":model}),
    )
    .unwrap();
    let passed = verdict["conclusion"] == "passed" && result["outcome"] == "complete";
    watch.0.kill().unwrap();
    watch.0.wait().unwrap();
    drop(env);
    upstream_runtime::finalize_with_retry(&f);
    let cleanup = queue::finalization::drive(&f.queue).unwrap();
    assert_eq!(cleanup.pending, 0, "{cleanup:?}");
    coord_broker.wait().unwrap();
    review_broker.wait().unwrap();
    assert!(!root.exists() && !coord.exists() && !f.temp.path().join("workspace").exists());
    assert!(!task_env::is_registered("task").unwrap());
    util::atomic_write_json(&f.temp.path().join("result.json"), &json!({"review_passed":passed,
        "model_request_sent":true,"task_resources_removed":true,"notification_ack_tested":false,
        "terminal_outcome":"failure (test teardown, not customer delivery)","global_installation_changed":false})).unwrap();
    assert!(passed, "real review did not pass; see preserved verdict");
}

fn attach(
    env: &mut task_env::TaskEnvironment,
    root: &Path,
    socket: &str,
    model: &J,
    cwd: &Path,
    review: Option<&str>,
) -> std::process::Child {
    let mut spec = json!({"adapter":"codex","configuration":"upstream","socket":socket,"cwd":cwd,
        "model":model,"approval_policy":"on-request","driver_binary":env!("CARGO_BIN_EXE_herdr-flow")});
    if let Some(review) = review {
        spec["review_snapshot"] = json!(review);
    }
    task_runtime::launch(env, root, &spec).unwrap()
}
