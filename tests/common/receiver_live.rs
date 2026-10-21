//! Opt-in real receiver proof; the driver never acknowledges or ticks the queue.
use super::*;
use herdr_flow::{opencode::session, util};
use std::time::{Duration, Instant};

struct FailureEvidence(Vec<std::path::PathBuf>);
impl Drop for FailureEvidence {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            return;
        }
        // Preserve only bounded diagnostic files, never the selected config/auth.
        let Ok(dir) = tempfile::Builder::new()
            .prefix("herdr-receiver-failure-")
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir()
        else {
            return;
        };
        for (index, source) in self.0.iter().enumerate() {
            use std::io::Read;
            if let Ok(file) = fs::File::open(source) {
                let mut bytes = Vec::new();
                if file.take(65536).read_to_end(&mut bytes).is_ok() {
                    let _ = fs::write(dir.path().join(format!("{index}.log")), bytes);
                }
            }
        }
        eprintln!(
            "bounded native failure evidence retained at {}",
            dir.keep().display()
        );
    }
}

// Explicit opt-in applies only to the disposable copy, never the selected file.
fn allow_owned_external_directory(config: &mut J, root: &Path) {
    let root = root.canonicalize().unwrap();
    let path = root.to_str().unwrap();
    assert!(!path.contains(['*', '?']), "owned path must be literal");
    let permissions = config
        .as_object_mut()
        .unwrap()
        .entry("permission")
        .or_insert_with(|| json!({}));
    let permissions = permissions
        .as_object_mut()
        .expect("scoped permission opt-in requires an object permission policy");
    let external = permissions
        .entry("external_directory")
        .or_insert_with(|| json!({}));
    let external = external
        .as_object_mut()
        .expect("scoped permission opt-in requires an object external_directory policy");
    external.insert(path.into(), json!("allow"));
    external.insert(format!("{path}/**"), json!("allow"));
}

#[test]
fn scoped_external_permission_preserves_other_rules_and_selected_config() {
    let temp = temp();
    let selected = json!({"permission":{"bash":"ask","external_directory":{"*":"ask","/unrelated/**":"deny"}},"model":"unchanged"});
    let mut copy = selected.clone();
    allow_owned_external_directory(&mut copy, temp.path());
    assert_eq!(copy["permission"]["bash"], "ask");
    assert_eq!(copy["permission"]["external_directory"]["*"], "ask");
    assert_eq!(
        copy["permission"]["external_directory"]["/unrelated/**"],
        "deny"
    );
    let root = temp.path().canonicalize().unwrap();
    assert_eq!(
        copy["permission"]["external_directory"][root.to_str().unwrap()],
        "allow"
    );
    assert_eq!(
        copy["permission"]["external_directory"][format!("{}/**", root.display())],
        "allow"
    );
    assert_eq!(copy["model"], selected["model"]);
    assert_eq!(
        selected["permission"]["external_directory"]
            .as_object()
            .unwrap()
            .len(),
        2
    );
}

fn acknowledged_delivery<'a>(state: &'a J, session: &str) -> Option<&'a str> {
    let deliveries = state["deliveries"].as_object()?;
    if deliveries.len() != 1 {
        return None;
    }
    let (id, delivery) = deliveries.iter().next()?;
    (delivery["phase"] == "acknowledged" && delivery["session"] == session).then_some(id.as_str())
}

#[test]
fn native_receipt_uses_durable_delivery_identity_after_watcher_moves_on() {
    let mut state = json!({"delivery_status":"waiting_for_events","deliveries":{
        "receipt":{"phase":"acknowledged","session":"sesreceiver"}}});
    assert_eq!(
        acknowledged_delivery(&state, "sesreceiver"),
        Some("receipt")
    );
    assert_eq!(acknowledged_delivery(&state, "other"), None);
    state["delivery_status"] = json!("acknowledged");
    state["deliveries"]["receipt"]["phase"] = json!("submitted");
    assert_eq!(acknowledged_delivery(&state, "sesreceiver"), None);
}

#[test]
#[ignore = "requires selected OpenCode 1.19.3 config/model and native shell permission"]
fn installed_real_opencode_receiver_readiness_and_ack() {
    run_native_receiver(false);
}

#[test]
#[ignore = "requires selected native model: real developer result and coordinator ACK/application"]
fn installed_real_opencode_developer_result_is_applied_by_native_receiver() {
    run_native_receiver(true);
}

fn run_native_receiver(apply_development: bool) {
    let _lock = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let temp = temp();
    let mut env = environment(temp.path());
    let cwd = env.workspace_root.join("receiver");
    fs::create_dir(&cwd).unwrap();
    let root = task_runtime::prepare(&env, "real-receiver").unwrap();
    let worker_root = apply_development.then(|| task_runtime::prepare(&env, "review").unwrap());
    let artifact_roots: Vec<_> = if apply_development {
        ["source", "acceptance"]
            .iter()
            .map(|name| {
                let path = env.workspace_root.join(name);
                common::create_dir_all(&path).unwrap();
                path
            })
            .collect()
    } else {
        Vec::new()
    };
    // Native launch restricts writes to this runtime root and cwd. Coordinator
    // state must live inside that boundary, not in a sibling of the runtime.
    let q = root.join("queue");
    let guard = root.join("guard");
    let mut evidence_paths = vec![
        root.join("server.log"),
        guard.join("foreground.log"),
        q.join("foreground.log"),
        q.join("queue.json"),
        q.join("readiness.json"),
        guard.join("submission.json"),
    ];
    if let Some(worker_root) = &worker_root {
        evidence_paths.extend([
            worker_root.join("server.log"),
            worker_root.join("submission.json"),
        ]);
    }
    let _failure_evidence = FailureEvidence(evidence_paths);
    let selected = std::env::var("HERDR_LIVE_OPENCODE_CONFIG").unwrap();
    let mut config = util::read_json(Path::new(&selected)).unwrap();
    // Only an explicit operator opt-in may widen the disposable copy's directory
    // permission. All other native approvals still block this bounded test.
    if std::env::var("HERDR_LIVE_ALLOW_OWNED_EXTERNAL_DIR").as_deref() == Ok("1") {
        allow_owned_external_directory(&mut config, &root);
        if let Some(worker_root) = &worker_root {
            // The coordinator reads the real collector report, but native kernel
            // confinement still prevents writes to this sibling worker runtime.
            allow_owned_external_directory(&mut config, worker_root);
        }
        for artifact_root in &artifact_roots {
            // Read-only to this coordinator at the kernel layer; these are
            // exact disposable test checkouts, never operator repositories.
            allow_owned_external_directory(&mut config, artifact_root);
        }
    }
    let instructions = cwd.join("receiver-instructions.md");
    let binary = util::shell_quote(env!("CARGO_BIN_EXE_herdr-flow"));
    let queue_path = util::shell_quote(q.to_str().unwrap());
    fs::write(&instructions, format!(
        "This is an authorized disposable receiver integration test. Do not delegate or create sessions. Upon receiving the queue notification, use its exact OPENCODE_SESSION_ID to execute the exact ACK command in that notification first. If it succeeds, run {binary} queue readiness --state-dir {queue_path} and require active_wakeup=true. An unacknowledged active delivery intentionally makes readiness false; do not treat that as permission to edit state. Retry only lock-busy errors for at most 15 seconds. Do not fabricate JSON or edit queue/watch files. ACK means receipt only. Then finish your turn. No other task work is authorized."
    )).unwrap();
    if apply_development {
        fs::write(&instructions, format!(
            "Authorized disposable coordinator test. Do not delegate or create sessions. Exactly one real development result for task review will arrive. Read the notification and exact bound collector evidence. Use the exact OPENCODE_SESSION_ID from the notification for every herdr-flow command. First execute its exact queue ack command. Then read current queue.json, task review state_version and pending event ID; read the actual collector report referenced by the event's evidence/result, using its output object unchanged. Write an application JSON in cwd with task=review, current state_version, exact event, output from that collector, acceptance={{goal: development assignment completed; independent acceptance still required}}, release_evidence pointing to {release}, next_action={{kind:complete}}. Run {binary} queue task-apply --state-dir {queue_path} --spec YOUR_APPLICATION_FILE. Do not edit queue, guard, reports, or tracked artifacts directly; do not fabricate output or verdicts. Development completion must leave project.approved null. Finally run {binary} queue readiness --state-dir {queue_path}, require active_wakeup=true, and finish. Retry only lock-busy errors for at most15seconds. The driver stops and reaps the developer immediately after production collection; before applying read its factual release receipt at {release}. If this receipt is not present yet, wait up to15seconds for that local file; do not infer release or fabricate it. This is not independent acceptance or delivery. Do not inspect herdr-flow source, operator repositories, or any paths outside the supplied disposable roots. The task-apply schema and command above are complete; write only your application JSON in cwd. No repository search is authorized.",
            release=root.join("developer-release.json").display()
        )).unwrap();
    }
    let items = config
        .as_object_mut()
        .unwrap()
        .entry("instructions")
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .unwrap();
    items.push(json!(instructions));
    common::create_dir_all(root.join("config/opencode")).unwrap();
    util::atomic_write_json(&root.join("config/opencode/opencode.json"), &config).unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener); // Native server requires an explicit port; failure is not retried.
    let model: J =
        serde_json::from_str(&std::env::var("HERDR_LIVE_OPENCODE_MODEL").unwrap()).unwrap();
    let mut server = task_runtime::launch(
        &mut env,
        &root,
        &json!({
            "adapter":"opencode","binary":std::env::var("HERDR_LIVE_OPENCODE_BINARY").unwrap(),
            "cwd":cwd,"model":model,"port":port
        }),
    )
    .unwrap();
    let endpoint = format!("http://127.0.0.1:{port}");
    let client: ureq::Agent = ureq::Agent::config_builder()
        .proxy(None)
        .timeout_global(Some(Duration::from_secs(2)))
        .build()
        .into();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        assert!(server.try_wait().unwrap().is_none(), "native server exited");
        if let Ok(mut response) = client.get(format!("{endpoint}/global/health")).call() {
            if let Ok(value) = response.body_mut().read_json::<J>() {
                if value["healthy"] == true {
                    break;
                }
            }
        }
        assert!(
            Instant::now() < deadline,
            "native server readiness deadline"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    let registered = session::create_fresh(
        &guard,
        &json!({"endpoint":endpoint,"cwd":cwd,"model":model}),
    )
    .unwrap();
    assert!(registered["session"]
        .as_str()
        .is_some_and(|s| !s.is_empty()));
    let mut guard_child = owned_foreground_watcher(&mut env, "opencode", &guard);
    let mut state = queue::init(&q, Some(&guard), 10).unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        match queue::enable(&q, &mut state, &herdr_flow::herdr::receiver_factory) {
            Ok(()) => break,
            Err(e) if e.kind == herdr_flow::error::ErrorKind::StateBusy => {
                assert!(Instant::now() < deadline, "{e}");
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => panic!("enable: {e}"),
        }
    }
    let development_cwd = apply_development.then(|| setup_in_queue(&env, "development", &q).1);
    let mut queue_child = owned_foreground_watcher(&mut env, "queue", &q);
    let development_result = if let Some(development_cwd) = development_cwd {
        git(&development_cwd, &["config", "user.name", "Native fixture"]);
        git(
            &development_cwd,
            &["config", "user.email", "native@example.invalid"],
        );
        let (worker, result) = acceptance_live::native(&mut env, &q, &development_cwd, "review",
            "Complete the authorized development assignment: change file to exactly version B and commit locally. Do not write an acceptance verdict. Stay within this checkout.");
        assert_ne!(worker["session"], registered["session"]);
        util::atomic_write_json(&root.join("developer-release.json"),
            &json!({"session":worker["session"],"runtime":worker_root,"status":"server stopped and reaped after terminal collection"})).unwrap();
        Some(result)
    } else {
        let _queue_lock = queue::locked(&q, true).unwrap();
        queue::add(
            &q,
            &json!({"id":"receiver-proof","task":"receiver-proof","epoch":1,
        "owner":"test-driver","reason":"verify native receipt","condition":"ACK only",
        "kind":"signal","timeout_seconds":240}),
        )
        .unwrap();
        queue::signal(
            &q,
            "receiver-proof",
            "complete",
            "Disposable notification; not task acceptance",
            None,
        )
        .unwrap();
        None
    };

    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        if let Ok(submission) = util::read_json(&guard.join("submission.json")) {
            assert_ne!(
                submission["native_status"], "blocked",
                "native permission/question blocks receiver; no automatic approval"
            );
        }
        let state = queue::load_queue(&q).unwrap();
        if let Some(delivery) =
            acknowledged_delivery(&state, registered["session"].as_str().unwrap())
        {
            if let Ok(readiness) = util::read_json(&q.join("readiness.json")) {
                if readiness["active_wakeup"] == true
                    && readiness["last_confirmed_receipt"]["delivery"] == delivery
                    && readiness["last_confirmed_receipt"]["session"] == registered["session"]
                {
                    if let Ok(submission) = util::read_json(&guard.join("submission.json")) {
                        let applied =
                            !apply_development || state["tasks"]["review"]["phase"] == "completed";
                        if submission["phase"] == "complete" && applied {
                            break;
                        }
                    }
                }
            }
        }
        assert!(
            Instant::now() < deadline,
            "native ACK deadline; approval/provider failure is not success"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
    let session_id = registered["session"].as_str().unwrap();
    let rows: J = client
        .get(format!("{endpoint}/session/{session_id}/message?limit=200"))
        .query("directory", cwd.to_str().unwrap())
        .call()
        .unwrap()
        .body_mut()
        .with_config()
        .limit(8 * 1024 * 1024)
        .read_json()
        .unwrap();
    let mut required_actions = vec!["queue ack", "queue readiness"];
    if apply_development {
        required_actions.push("queue task-apply");
    }
    for action in required_actions {
        let observed = rows
            .as_array()
            .unwrap()
            .iter()
            .filter(|row| row["info"]["sessionID"] == session_id)
            .flat_map(|row| row["parts"].as_array().into_iter().flatten())
            .any(|part| {
                part["type"] == "tool"
                    && part["tool"] == "bash"
                    && part["state"]["status"] == "completed"
                    && part["state"]["input"]["command"]
                        .as_str()
                        .is_some_and(|command| {
                            command.contains(action) && command.contains(q.to_str().unwrap())
                        })
            });
        assert!(
            observed,
            "native session lacks a completed shell invocation of {action}"
        );
    }
    if let Some(result) = development_result {
        let state = queue::load_queue(&q).unwrap();
        assert!(state["tasks"]["project"]["approved"].is_null());
        assert_eq!(state["tasks"]["review"]["pending"], json!([]));
        let output = &result["output"];
        assert_eq!(
            git(
                Path::new(output["location"].as_str().unwrap()),
                &["show", &format!("{}:file", output["id"].as_str().unwrap())]
            ),
            "version B"
        );
        assert_eq!(
            state["tasks"]["project"]["artifacts"][output["id"].as_str().unwrap()]["object"],
            *output
        );
        let events = state["events"].as_object().unwrap();
        assert_eq!(
            events.len(),
            1,
            "exactly one real developer result expected"
        );
        let (event_id, event) = events.iter().next().unwrap();
        assert_eq!(event["wait"], "review");
        assert_eq!(event["binding"], result["context"]["binding"]);
        assert_eq!(event["outcome"], result["outcome"]);
        assert_eq!(event["evidence"], json!(q.join("results/review.json")));
        let signal = util::read_json(Path::new(event["evidence"].as_str().unwrap())).unwrap();
        assert_eq!(
            signal["evidence"],
            json!(worker_root
                .as_ref()
                .unwrap()
                .join("submissions")
                .join(format!("{}.json", result["message_id"].as_str().unwrap())))
        );
        let report = util::read_json(Path::new(signal["evidence"].as_str().unwrap())).unwrap();
        assert_eq!(report["output"], *output);
        assert_eq!(report["session"], result["session"]);
        assert_eq!(event["acknowledged"], true);
        assert_eq!(event["applied"], true);
        let delivery = acknowledged_delivery(&state, session_id).unwrap();
        assert_eq!(state["deliveries"][delivery]["events"], json!([event_id]));
    }
    destroy_environment(&env).unwrap();
    for child in [&mut server, &mut guard_child, &mut queue_child] {
        child.wait().unwrap();
    }
    assert!(!env.workspace_root.exists());
    assert!(!env.temp_root.exists());
    assert_eq!(occupied_slots().unwrap(), 0);
}
