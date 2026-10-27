//! Opt-in real Herdr/model diagnosis and repair. Setup, fault injection and final failure
//! are harness-owned; the recovery conclusion and coordinator response are real.
use super::*;
use std::time::{Duration, Instant};

fn herdr(args: &[&str]) -> J {
    let output = Command::new("herdr").args(args).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice::<J>(&output.stdout).unwrap()["result"].clone()
}

fn await_value(mut observe: impl FnMut() -> Option<J>) -> J {
    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        if let Some(value) = observe() {
            return value;
        }
        assert!(
            Instant::now() < deadline,
            "acceptance observation deadline; inspect without replay"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn turn(client: &mut herdr_flow::codex::rpc::Client, session: &J, id: &J) -> Option<J> {
    let reply = client
        .call(
            "thread/read",
            &json!({
                "threadId":session,"includeTurns":true
            }),
        )
        .unwrap();
    reply["thread"]["turns"]
        .as_array()
        .unwrap()
        .iter()
        .find(|turn| turn["id"] == *id && turn["status"] == "completed")
        .cloned()
}

fn screen(pane: &str, expected: &str) -> J {
    await_value(|| {
        let output = Command::new("herdr")
            .args([
                "agent",
                "read",
                pane,
                "--source",
                "recent-unwrapped",
                "--lines",
                "160",
            ])
            .output()
            .unwrap();
        if !output.status.success()
            && String::from_utf8_lossy(&output.stderr).contains("agent_not_idle")
        {
            return None;
        }
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        // Herdr read commands return rendered text; get/list return JSON.
        let text = String::from_utf8(output.stdout).unwrap();
        text.contains(expected).then(|| json!(text))
    })
}

#[test]
#[ignore = "explicit existing socket, role models and native binary; creates a private Herdr recovery task"]
fn stopped_watcher_real_diagnosis_handoff_and_cleanup() {
    exercise(false);
}

#[test]
#[ignore = "explicit existing socket, role models and native binary; repairs a private Herdr task watcher"]
fn stopped_watcher_real_repair_verification_handoff_and_cleanup() {
    exercise(true);
}

fn verify_repair(root: &Path, incident: &J, original: &J, state: &J) {
    assert_eq!(incident["diagnosis"]["disposition"], "repair_requested");
    assert_eq!(
        incident["diagnosis"]["requested_action"],
        "restart_receiver_watch"
    );
    assert_ne!(incident["diagnosis_turn"], incident["turn"]);
    assert_eq!(incident["repair"]["phase"], "verified", "{incident}");
    assert_eq!(incident["result"]["disposition"], "recovered", "{incident}");
    assert_eq!(incident["result"]["requested_action"], "none");
    assert_eq!(incident["repair"]["operation_replay_allowed"], false);
    assert_eq!(incident["repair"]["business_outcome"], "unchanged");
    assert_eq!(
        incident["repair"]["original_operation"]["binding"],
        json!({"session":state["session"],"turn":original["turn"]})
    );
    let guard = root.join("guard");
    let health = herdr_flow::watch_service::health(&guard);
    assert_eq!(health["healthy"], true, "{health}");
    assert_eq!(health["pid"], incident["repair"]["watcher"]["pid"]);
    assert_eq!(
        health["start_ticks"],
        incident["repair"]["watcher"]["start_ticks"]
    );
    assert_ne!(health["pid"], incident["fault"]["identity"]["pid"]);
    let activation = util::read_state_json(&guard, "activation.json").unwrap();
    assert_eq!(activation["pid"], health["pid"]);
    assert_eq!(activation["session"], state["session"]);
    assert_eq!(activation["recovery_incident"], incident["id"]);
    let env = task_env::restore_environment("task").unwrap();
    let pid = health["pid"].as_i64().unwrap() as i32;
    let ticks = health["start_ticks"].as_str().unwrap().parse().unwrap();
    assert_eq!(
        env.child_processes
            .iter()
            .filter(|p| p.pid == pid && p.start_time == Some(ticks))
            .count(),
        1,
        "the replacement watcher must be registered exactly once under the original task"
    );
    assert_eq!(task_env::process_start_time(pid), Some(ticks));
}

fn exercise(repair: bool) {
    let socket = std::env::var("HERDR_UPSTREAM_SOCKET").unwrap();
    let model: J = serde_json::from_str(&std::env::var("HERDR_UPSTREAM_MODEL").unwrap()).unwrap();
    let roles: J =
        serde_json::from_str(&std::env::var("HERDR_UPSTREAM_ROLE_MODELS").unwrap()).unwrap();
    assert!(
        roles["recovery"].is_object(),
        "explicit recovery selection required"
    );
    let binary = std::env::var("HERDR_NATIVE_BINARY").unwrap();
    let session_store = std::env::var("HERDR_SESSION_STORE").unwrap();
    let config_path = std::env::var("HERDR_UPSTREAM_CONFIG").unwrap();
    let config_before = fs::read(&config_path).unwrap();
    let parent = std::env::var("HERDR_PANE_ID").unwrap();
    let mut client = herdr_flow::codex::rpc::Client::new(&socket);
    client.timeout = 5.0;
    let parent_before = herdr(&["pane", "get", &parent])["pane"].clone();
    // A failed setup/cleanup must retain the exact ownership record for recovery.
    let mut f = Fixture::new();
    // Retain evidence on failure without leaking the fixture mutex or its
    // task-registry environment override when both live cases run together.
    f.temp.disable_cleanup(true);
    eprintln!("recovery live root: {}", f.temp.path().display());
    let mut env = f.ready();
    let root = task_runtime::prepare(&env, "coordinator").unwrap();
    let workspace = env.workspace_root.clone();
    let scratch = env.temp_root.clone();
    let environment_file = env.state_file().to_path_buf();
    let mut broker = task_runtime::launch(
        &mut env,
        &root,
        &json!({
            "adapter":"codex","configuration":"upstream","socket":socket,
            "cwd":workspace.join("checkout"),"model":model,"role_models":roles,
            "approval_policy":"on-request","driver_binary":env!("CARGO_BIN_EXE_herdr-flow"),
        "native_tui":{"binary":binary,"parent_pane":parent,"session_store":session_store}
        }),
    )
    .unwrap();
    let guard = root.join("guard");
    let state = task_runtime::native::receiver::bootstrap(
        &env,
        &root,
        &json!({"guard_dir":guard,"task":"task","role":"coordinator"}),
    )
    .unwrap();
    let activated = task_runtime::native::receiver::active::activate(
        &mut env,
        &root,
        &json!({"driver_binary":env!("CARGO_BIN_EXE_herdr-flow")}),
    )
    .unwrap();
    {
        let _lock = queue::locked(&f.queue, false).unwrap();
        let mut q = f.state();
        q["guard_dir"] = json!(guard);
        q["receiver"] = guard::scope(&util::read_state_json(&guard, "state.json").unwrap());
        queue::save(&f.queue, &mut q).unwrap();
    }
    let log = fs::File::create(f.temp.path().join("queue-watch.log")).unwrap();
    let watch = FixtureChild(
        Command::new(env!("CARGO_BIN_EXE_herdr-flow"))
            .args(["queue", "watch", "--state-dir"])
            .arg(&f.queue)
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap(),
    );
    await_value(|| {
        let health = herdr_flow::watch_service::health(&queue::artifacts(&f.queue).unwrap());
        (health["healthy"] == true).then_some(health)
    });
    let mut held_env = Some(env);
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let env = held_env.as_ref().unwrap();
        assert_eq!(
            activated["recovery"]["agent"]["phase"], "ready",
            "{activated}"
        );
        let started = task_runtime::native::receiver::start(env, &root, &json!({
            "instruction":"This is scoped Recovery Agent acceptance. Reply READY RECOVERY-LIVE. Do not use tools, modify files, change queue state, finalize, or start/stop any operation. If a later recovery report arrives, summarize its facts, actual actions, current state and next step, explicitly preserving unknown original operations without replay. Remain waiting; the harness owns terminal cleanup."
        })).unwrap();
        assert_eq!(started["presentation"]["phase"], "ready", "{started}");
        await_value(|| turn(&mut client, &state["session"], &started["turn"]));
        let original = util::read_state_json(&root, "receiver-start.json").unwrap();
        let registration = util::read_state_json(&root, "receiver.json").unwrap();
        let watcher = activated["pid"].as_u64().unwrap() as i32;
        assert!(env.owns_process(watcher));
        let identity = util::read_state_json(&guard, "watch.json").unwrap();
        assert_eq!(identity["pid"], watcher);
        assert_eq!(
            identity["start_ticks"].as_str(),
            util::process::process_start_ticks(watcher as u32).as_deref()
        );
        assert!(Command::new("kill")
            .args(["-TERM", &watcher.to_string()])
            .status()
            .unwrap()
            .success());
        if repair {
            drop(held_env.take());
        }
        eprintln!(
            "stopped owned watcher {watcher}; environment lock held: {}",
            held_env.is_some()
        );
        let incident = await_value(|| {
            let status = task_runtime::recovery::status(&root).unwrap();
            util::atomic_write_json(&f.temp.path().join("latest-recovery.json"), &status).unwrap();
            assert_ne!(status["incident"]["phase"], "unavailable", "{status}");
            (status["incident"]["phase"] == "handled").then(|| status["incident"].clone())
        });
        assert_eq!(incident["fault"]["kind"], "watcher_unavailable");
        assert_eq!(incident["fault"]["source"], json!(guard));
        assert_eq!(incident["fault"]["operation_replay_allowed"], false);
        assert_ne!(incident["agent"]["session"], state["session"]);
        assert_eq!(incident["agent"]["model"], roles["recovery"]);
        if repair {
            verify_repair(&root, &incident, &original, &state);
        } else {
            assert!(
                matches!(
                    incident["result"]["disposition"].as_str(),
                    Some("needs_human" | "insufficient_evidence")
                ),
                "{incident}"
            );
            assert!(incident["repair"]["watcher"].is_null(), "{incident}");
        }
        for key in ["facts", "actions", "current_state", "next_step"] {
            assert!(!incident["result"][key].as_str().unwrap().trim().is_empty());
        }
        assert_eq!(incident["presentation"]["phase"], "ready", "{incident}");
        assert_eq!(
            incident["presentation"]["workspace"],
            parent_before["workspace_id"]
        );
        assert_ne!(incident["presentation"]["tab"], parent_before["tab_id"]);
        assert_ne!(
            incident["presentation"]["tab"],
            started["presentation"]["tab"]
        );
        let pane = incident["presentation"]["pane"].as_str().unwrap();
        let agent = herdr(&["agent", "get", pane]);
        assert!(agent
            .to_string()
            .contains(incident["agent"]["session"].as_str().unwrap()));
        let expected = if repair {
            "recovered"
        } else {
            incident["result"]["incident"].as_str().unwrap()
        };
        let visible = screen(pane, expected);
        assert!(visible
            .as_str()
            .unwrap()
            .contains(incident["id"].as_str().unwrap()));
        if repair {
            assert!(visible
                .as_str()
                .unwrap()
                .contains(&incident["repair"]["watcher"]["pid"].to_string()));
        }
        let handoff =
            await_value(|| turn(&mut client, &state["session"], &incident["handoff"]["turn"]));
        assert_eq!(
            util::read_state_json(&root, "receiver-start.json").unwrap(),
            original
        );
        assert_eq!(
            util::read_state_json(&root, "receiver.json").unwrap(),
            registration
        );
        assert!(f.state()["tasks"]["task"]["final_outcome"].is_null());
        let recovery_thread = client
            .call(
                "thread/read",
                &json!({"threadId":incident["agent"]["session"],"includeTurns":true}),
            )
            .unwrap();
        assert_eq!(
            recovery_thread["thread"]["turns"].as_array().unwrap().len(),
            if incident["diagnosis"].is_null() {
                1
            } else {
                2
            }
        );
        let coordinator_thread = client
            .call(
                "thread/read",
                &json!({"threadId":state["session"],"includeTurns":true}),
            )
            .unwrap();
        assert_eq!(
            coordinator_thread["thread"]["turns"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        util::atomic_write_json(&f.temp.path().join("result.json"), &json!({
            "incident":incident,"recovery_agent":agent,"recovery_screen":visible,"coordinator_response":handoff,
            "original":original,"coordinator_presentation":started["presentation"],
            "environment_lock_held":held_env.is_some(),"original_not_replayed":true,
            "recovery_thread":recovery_thread,"coordinator_thread":coordinator_thread
        })).unwrap();
    }));
    let processes = util::read_json(&environment_file).unwrap()["child_processes"].clone();
    util::atomic_write_json(&f.temp.path().join("processes.json"), &processes).unwrap();
    let mut inventory = Vec::new();
    for entry in fs::read_dir(scratch.join("rt")).unwrap() {
        let directory = entry.unwrap().path();
        inventory.push(
            json!({"runtime":util::read_state_json(&directory,"runtime.json").unwrap(),
            "session":util::read_state_json(&directory,"upstream-session.json").ok(),
            "presentation":util::read_state_json(&directory,"presentation.json").ok()}),
        );
    }
    util::atomic_write_json(&f.temp.path().join("inventory.json"), &json!(inventory)).unwrap();
    if outcome.is_err() {
        let mut logs = json!({});
        for entry in fs::read_dir(scratch.join("rt")).unwrap() {
            let directory = entry.unwrap().path();
            for name in [
                "broker.log",
                "server.log",
                "runtime.json",
                "presentation.json",
            ] {
                if let Ok(text) = fs::read_to_string(directory.join(name)) {
                    logs[format!(
                        "{}/{}",
                        directory.file_name().unwrap().to_string_lossy(),
                        name
                    )] = json!(text);
                }
            }
        }
        util::atomic_write_json(&f.temp.path().join("failure.json"), &logs).unwrap();
    }
    // This explicitly selected fixture failure closes the diagnostic scenario;
    // neither a diagnosis nor a stopped watcher is a business success/failure.
    drop(held_env);
    upstream_runtime::finalize_with_retry(&f);
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let cleanup = queue::finalization::drive(&f.queue).unwrap();
        if cleanup.pending == 0 {
            break;
        }
        assert!(
            cleanup.errors.iter().all(
                |e| e.contains("close is not persisted") || e.contains("lifecycle action busy")
            ),
            "{cleanup:?}"
        );
        assert!(Instant::now() < deadline, "{cleanup:?}");
        std::thread::sleep(Duration::from_millis(100));
    }
    broker.wait().unwrap();
    drop(watch);
    assert!(!workspace.exists() && !scratch.exists());
    assert!(!task_env::is_registered("task").unwrap());
    assert!(f.state()["tasks"]["task"].is_null());
    for process in processes.as_array().unwrap() {
        let record: task_env::RegisteredProcess = serde_json::from_value(process.clone()).unwrap();
        assert!(
            !task_env::process_is_alive(&record),
            "owned process survived cleanup: {process}"
        );
    }
    assert!(
        fs::read(&config_path).unwrap() == config_before,
        "upstream user configuration changed"
    );
    assert!(!git(
        &f.temp.path().join("repo"),
        &["worktree", "list", "--porcelain"]
    )
    .contains("workspace/checkout"));
    assert_eq!(
        herdr(&["pane", "get", &parent])["pane"]["terminal_id"],
        parent_before["terminal_id"]
    );
    util::atomic_write_json(
        &f.temp.path().join("cleanup.json"),
        &json!({
            "roots_removed":true,"queue_task_removed":true,"registration_removed":true,
            "worktree_removed":true,"parent_terminal_unchanged":true,"owned_processes_removed":true,
            "final_outcome":"harness-selected failure; no business delivery claim"
        }),
    )
    .unwrap();
    if let Err(error) = outcome {
        std::panic::resume_unwind(error);
    }
}
