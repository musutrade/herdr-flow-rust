//! Explicit real-model review of a fixed local snapshot. This is a caller-driven
//! acceptance scenario, not a claim of autonomous notification/ACK/publication.
use super::*;

#[test]
#[ignore = "explicit existing endpoint/model: coordinator develops, reviews, delivers and finalizes a new local task"]
fn live_upstream_coordinator_full_task() {
    let socket = std::env::var("HERDR_UPSTREAM_SOCKET").expect("existing socket required");
    let model: J = serde_json::from_str(&std::env::var("HERDR_UPSTREAM_MODEL").unwrap()).unwrap();
    let f = std::mem::ManuallyDrop::new(Fixture::new());
    eprintln!("full task evidence root: {}", f.temp.path().display());
    let target = f.temp.path().join("output.git");
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
    let goal = "Create and commit a tracked file answer.txt containing exactly 42 followed by one newline. Independently verify the fixed commit and deliver it to the predeclared local output repository.";
    let mut env = isolated_runtime::linked_delivery(&f, goal, Some(&target));
    let coord = task_runtime::prepare(&env, "coordinator").unwrap();
    let cwd = env.workspace_root.join("checkout");
    let workspace = env.workspace_root.clone();
    let scratch = env.temp_root.clone();
    let role_models: J = serde_json::from_str(
        &std::env::var("HERDR_UPSTREAM_ROLE_MODELS")
            .expect("explicit developer/reviewer models required"),
    )
    .unwrap();
    let mut broker = task_runtime::launch(
        &mut env,
        &coord,
        &json!({
            "adapter":"codex","configuration":"upstream","socket":socket,"cwd":cwd,
            "model":model,"role_models":role_models,"approval_policy":"on-request",
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
    bind_coordinator(&f, &guard);
    let instruction = full_task_instruction(&f, &target);
    // Start before the external queue supervisor to avoid a queue-lock race.
    // The coordinator only receives bounded tools; no worker can dispatch until
    // the watcher processes its request and readiness has succeeded.
    task_runtime::native::receiver::start(&env, &coord, &json!({"instruction":instruction}))
        .unwrap();
    drop(env);
    let log = fs::File::create(f.temp.path().join("full-task-watch.log")).unwrap();
    let mut watch = FixtureChild(
        Command::new(env!("CARGO_BIN_EXE_herdr-flow"))
            .args(["queue", "watch", "--state-dir"])
            .arg(&f.queue)
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap(),
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(600);
    while !f.state()["tasks"]["task"].is_null() {
        assert!(
            watch.0.try_wait().unwrap().is_none(),
            "queue supervisor stopped; retain state for inspected recovery"
        );
        assert!(
            std::time::Instant::now() < deadline,
            "real task deadline; retain state without redispatch"
        );
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
    watch.0.kill().unwrap();
    watch.0.wait().unwrap();
    broker.wait().unwrap();
    assert!(!workspace.exists() && !scratch.exists());
    assert!(!task_env::is_registered("task").unwrap());
    assert_upstream_empty(&socket, &[cwd.clone(), workspace.join("reviews/review-a1")]);
    assert_eq!(
        git(&target, &["show", "refs/heads/accepted:answer.txt"]),
        "42"
    );
    let output = Command::new("git")
        .arg("-C")
        .arg(&target)
        .args(["show", "refs/heads/accepted:answer.txt"])
        .output()
        .unwrap();
    assert_eq!(output.stdout, b"42\n");
    util::atomic_write_json(
        &f.temp.path().join("full-task-result.json"),
        &json!({
        "scope":"real coordinator/developer/reviewer; new predeclared local Git target only",
        "candidate_seeded_by_harness":false,"model":model,"role_models":role_models,"task_resources_removed":true,
        "commit":git(&target, &["rev-parse","refs/heads/accepted"]),
        "global_entrypoint_changed":false,"remote_publication":false}),
    )
    .unwrap();
}

fn assert_upstream_empty(socket: &str, cwds: &[PathBuf]) {
    let mut client = herdr_flow::codex::rpc::Client::new(socket);
    for archived in [false, true] {
        let result = client
            .call(
                "thread/list",
                &json!({"cwd":cwds,"archived":archived,
            "sourceKinds":["cli","vscode","exec","appServer","subAgent","subAgentReview",
                "subAgentCompact","subAgentThreadSpawn","subAgentOther","unknown"],
            "useStateDbOnly":true,"limit":128}),
            )
            .unwrap();
        assert!(
            result["data"].as_array().unwrap().is_empty() && result["nextCursor"].is_null(),
            "owned task cwd retains upstream sessions: {}",
            result["data"]
                .as_array()
                .unwrap()
                .iter()
                .map(|t| t["id"].clone())
                .collect::<Vec<_>>()
                .len()
        );
    }
}

fn bind_coordinator(f: &Fixture, guard: &Path) {
    let _lock = queue::locked(&f.queue, false).unwrap();
    let state = util::read_state_json(guard, "state.json").unwrap();
    let mut q = f.state();
    q["guard_dir"] = json!(guard);
    q["receiver"] = guard::scope(&state);
    q["notify"] = json!(true);
    q["poll_seconds"] = json!(1);
    queue::save(&f.queue, &mut q).unwrap();
}

fn full_task_instruction(f: &Fixture, target: &Path) -> String {
    format!(
        r#"Complete only registered task task through herdr_queue tools. Inspect its actual requirement and existing permissions. Use the already registered development wait dev: run with instruction to create answer.txt containing exactly 42 plus newline and commit it. The developer may use git -c user.name=Acceptance -c user.email=acceptance@example.test commit (per-command identity only). No network, login or global configuration changes. Do not edit the candidate yourself.
After dispatch stop until the result notification. Inspect runtime_results for the development output; ACK that delivery and apply the dev event with output, acceptance {{"goal":"bound development result completed"}}, release_evidence identifying the completed turn and next_action {{"kind":"complete"}}. Select that output's full SHA using lineage kind candidate with parent task task and its CURRENT state_version. Register independent acceptance with lineage kind activity, task task, current state_version, activity review, role acceptance, input full SHA, session_epoch 1, result_wait {{"id":"review","kind":"signal","task":"review","epoch":1,"assignment":"review-a1","owner":"reviewer","reason":"independent fixed snapshot review","condition":"report","timeout_seconds":600}}.
Run wait review with an instruction to verify the exact tracked bytes and write .herdr-verdict.json using the supplied acceptance_protocol, exact original requirement source, criterion goal, no tracked edits, no git commits. Stop until notification. ACK and apply its immutable verdict; a passed report uses next_action {{"kind":"delivery","owner":"coordinator","reason":"independently passed","condition":"existing local delivery authorization"}}. Never supply reviewer acceptance evidence yourself. If a real defect occurs use existing continue/repair rules with new activities and independent re-review, never overwrite a verdict.
For a passed review, deliver spec {{"adapter":"git","target":{},"ref":"refs/heads/accepted","expected":{},"authorization":"deliver-local","state_version":CURRENT_PARENT_VERSION}}. This authorization covers only this predeclared new local repository. Inspect again after delivery for the current parent version; finalize task task with outcome success only after confirmed delivery, and a reason. On an actual unrecoverable task failure, finalize failure with a concrete reason. Do not invent success. For unknown external outcomes stop and report without replay. Use inspect for current versions after mutations. Do not poll or use shell for queue operations."#,
        json!(target),
        json!(f.commit)
    )
}

#[test]
#[ignore = "explicit existing service/model; one real review turn in a new private queue"]
fn live_upstream_fixed_snapshot_review() {
    run(false);
}

#[test]
#[ignore = "explicit existing service/model; real reviewer and coordinator notification turns"]
fn live_upstream_notification_ack_application() {
    run(true);
}

fn run(notifications: bool) {
    let socket = std::env::var("HERDR_UPSTREAM_SOCKET").expect("existing socket required");
    let model: J = serde_json::from_str(&std::env::var("HERDR_UPSTREAM_MODEL").unwrap()).unwrap();
    let f = std::mem::ManuallyDrop::new(Fixture::new());
    eprintln!("real review evidence root: {}", f.temp.path().display());
    let goal = "The tracked file named file must contain exactly review candidate followed by one newline.";
    let target = f.temp.path().join("output.git");
    let mut env =
        isolated_runtime::linked_delivery(&f, goal, notifications.then_some(target.as_path()));
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
        q["poll_seconds"] = json!(1);
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
    // Preserve the historical caller-driven scenario separately.
    if !notifications {
        let _lock = queue::locked(&f.queue, false).unwrap();
        let mut q = f.state();
        q["notify"] = json!(false);
        queue::save(&f.queue, &mut q).unwrap();
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(180);
    let result = loop {
        let result = match task_runtime::native::collect(&env, &root) {
            Err(error)
                if error.kind == herdr_flow::ErrorKind::StateBusy
                    && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(std::time::Duration::from_millis(50));
                continue;
            }
            result => result.unwrap(),
        };
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
        "collection":"caller-driven","notification_delivery":if notifications {"pending; see notification-application.json"} else {"disabled in this scenario"},"model":model}),
    )
    .unwrap();
    let passed = verdict["conclusion"] == "passed" && result["outcome"] == "complete";
    if notifications {
        await_application(&f.queue, f.temp.path(), &coord, &mut watch);
    }
    watch.0.kill().unwrap();
    watch.0.wait().unwrap();
    if notifications {
        deliver_local(&f, &mut env, &target);
    }
    drop(env);
    upstream_runtime::finalize_with_outcome(&f, if notifications { "success" } else { "failure" });
    let cleanup = queue::finalization::drive(&f.queue).unwrap();
    assert_eq!(cleanup.pending, 0, "{cleanup:?}");
    coord_broker.wait().unwrap();
    review_broker.wait().unwrap();
    assert!(!root.exists() && !coord.exists() && !f.temp.path().join("workspace").exists());
    assert!(!task_env::is_registered("task").unwrap());
    util::atomic_write_json(&f.temp.path().join("result.json"), &json!({"review_passed":passed,
        "model_request_sent":true,"task_resources_removed":true,"notification_ack_tested":notifications,
        "terminal_outcome":if notifications {"success (isolated local output only)"} else {"failure (test teardown)"},"global_installation_changed":false})).unwrap();
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

fn await_application(queue_root: &Path, evidence: &Path, coord: &Path, watch: &mut FixtureChild) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(180);
    loop {
        assert!(
            watch.0.try_wait().unwrap().is_none(),
            "queue watcher exited"
        );
        let q = queue::load_queue(queue_root).unwrap();
        let applied =
            q["events"].as_object().unwrap().values().any(|e| {
                e["task"] == "review" && e["acknowledged"] == true && e["applied"] == true
            });
        if applied {
            util::atomic_write_json(&evidence.join("notification-application.json"), &q).unwrap();
            let tools = util::read_state_json(coord, "control-tools.json").unwrap();
            util::atomic_write_json(&evidence.join("coordinator-tools.json"), &tools).unwrap();
            for action in ["ack", "apply"] {
                assert!(tools
                    .as_object()
                    .unwrap()
                    .values()
                    .any(|t| t["request"]["params"]["arguments"]["action"] == action
                        && t["result"]["success"] == true));
            }
            assert_eq!(
                q["tasks"]["task"]["continuation"]["action"]["kind"],
                "delivery"
            );
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "coordinator ACK/application timed out; preserve evidence"
        );
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}

#[test]
#[ignore = "resume only an explicitly retained review; no repeat dispatch"]
fn resume_upstream_notification_acceptance() {
    let evidence = PathBuf::from(std::env::var("HERDR_UPSTREAM_RESUME").unwrap());
    let queue_root = evidence.join("queue");
    std::env::set_var(
        "HERDR_TASK_ENV_ROOT",
        queue_root.join("queue.json/task-environments"),
    );
    let env = task_env::restore_environment("task").unwrap();
    let root = env.temp_root.join("rt/review-a1");
    let coord = env.temp_root.join("rt/coordinator");
    let log = fs::File::create(evidence.join("queue-watch-resumed.log")).unwrap();
    let mut watch = FixtureChild(
        Command::new(env!("CARGO_BIN_EXE_herdr-flow"))
            .args(["queue", "watch", "--state-dir"])
            .arg(&queue_root)
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap(),
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        match task_runtime::native::collect(&env, &root) {
            Ok(result) if result["phase"] == "signalled" => break,
            Ok(_) => (),
            Err(e) if e.kind == herdr_flow::ErrorKind::StateBusy => (),
            Err(e) => panic!("{e}"),
        }
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    await_application(&queue_root, &evidence, &coord, &mut watch);
    watch.0.kill().unwrap();
    watch.0.wait().unwrap();
    drop(env);
    // Cleanup is performed by the explicit retained-acceptance recovery entry.
}

fn deliver_local(f: &Fixture, env: &mut task_env::TaskEnvironment, target: &Path) {
    use herdr_flow::delivery;
    git(
        f.temp.path(),
        &[
            "clone",
            "--bare",
            f.temp.path().join("repo").to_str().unwrap(),
            target.to_str().unwrap(),
        ],
    );
    git(target, &["update-ref", "refs/heads/accepted", &f.commit]);
    let q = f.state();
    let spec = json!({"adapter":"git","target":target,"ref":"refs/heads/accepted","expected":f.commit,
        "authorization":"deliver-local","state_version":q["tasks"]["task"]["state_version"]});
    delivery::prepare(env, &f.queue, &spec, &delivery::local::Local).unwrap();
    let receipt = delivery::execute(env, &delivery::local::Local).unwrap();
    util::atomic_write_json(&f.temp.path().join("local-delivery.json"), &receipt).unwrap();
    assert_eq!(receipt["phase"], "confirmed");
    assert_eq!(
        git(target, &["rev-parse", "refs/heads/accepted"]),
        q["tasks"]["task"]["candidate"].as_str().unwrap()
    );
    assert_eq!(
        git(target, &["show", "refs/heads/accepted:file"]),
        "review candidate"
    );
}
