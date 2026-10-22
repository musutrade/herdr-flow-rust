//! Opt-in real receiver proof; the driver never acknowledges or ticks the queue.
use super::*;
use herdr_flow::{opencode::session, util};
use std::path::PathBuf;
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
    run_native_receiver(false, false, false, false, false);
}

#[test]
#[ignore = "requires selected native model: real developer result and coordinator ACK/application"]
fn installed_real_opencode_developer_result_is_applied_by_native_receiver() {
    run_native_receiver(true, false, false, false, false);
}

#[test]
#[ignore = "requires real developer, independent reviewer and native coordinator application"]
fn installed_real_opencode_independent_acceptance_is_applied_by_native_receiver() {
    run_native_receiver(true, true, false, false, false);
}

#[test]
#[ignore = "requires accepted native candidate and external unmarked Local Git delivery"]
fn installed_real_opencode_accepted_result_is_delivered_by_external_driver() {
    run_native_receiver(true, true, true, false, false);
}

#[test]
#[ignore = "requires real failed review, repair, fresh review and external Local Git delivery"]
fn installed_real_opencode_failed_review_is_repaired_and_delivered_by_external_driver() {
    run_native_receiver(true, true, true, true, false);
}

#[test]
#[ignore = "requires confirmed external delivery and fresh unmarked supervisor restart"]
fn installed_real_opencode_confirmed_delivery_restarts_in_external_supervisor() {
    run_native_receiver(true, true, true, true, true);
}

#[test]
#[ignore = "requires two completed real native rounds: normal delivery then repair/restart on one outer base"]
fn installed_real_opencode_two_rounds_share_one_outer_base() {
    let temp = temp();
    let base = temp.path();
    let round1 = base.join("round-1");
    let round2 = base.join("round-2");
    common::create_dir(&round1).unwrap();
    common::create_dir(&round2).unwrap();
    let formal1 = round1.join("formal.git");
    let user1 = round1.join("user-owned.txt");
    let formal2 = round2.join("formal.git");
    let user2 = round2.join("user-owned.txt");
    let excluded = vec![
        formal1.clone(),
        user1.clone(),
        formal2.clone(),
        user2.clone(),
    ];

    run_native_receiver_in(
        true, true, true, false, false, base, &formal1, &user1, &excluded,
    );
    let commit1 = git(&formal1, &["rev-parse", "refs/heads/main"]);
    let identity1 = user_identity(&user1);
    assert_round_output(&formal1, &user1, &commit1, identity1);

    run_native_receiver_in(
        true, true, true, true, true, base, &formal2, &user2, &excluded,
    );
    let commit2 = git(&formal2, &["rev-parse", "refs/heads/main"]);
    let identity2 = user_identity(&user2);
    assert_round_output(&formal1, &user1, &commit1, identity1);
    assert_round_output(&formal2, &user2, &commit2, identity2);
    assert!(base.join("registry").exists());
    assert!(!base.join("workspace").exists());
    assert!(!base.join("temporary").exists());
    assert!(!is_registered("project").unwrap());
    assert_eq!(occupied_slots().unwrap(), 0);
    let reports = [
        base.join("inventory-before.json"),
        base.join("inventory-after.json"),
    ];
    for report in &reports {
        let len = fs::metadata(report).unwrap().len();
        assert!(len > 0 && len <= 1024 * 1024, "unbounded report");
    }
    let report_count = fs::read_dir(base)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("inventory-")
        })
        .count();
    assert_eq!(report_count, 2, "reports must remain a rolling pair");
}

fn user_identity(path: &Path) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    let metadata = fs::metadata(path).unwrap();
    (metadata.dev(), metadata.ino())
}

fn assert_round_output(target: &Path, user_asset: &Path, commit: &str, identity: (u64, u64)) {
    use std::os::unix::fs::MetadataExt;
    assert_eq!(git(target, &["rev-parse", "refs/heads/main"]), commit);
    assert_eq!(git(target, &["show", "refs/heads/main:file"]), "version B");
    let metadata = fs::metadata(user_asset).unwrap();
    assert_eq!((metadata.dev(), metadata.ino()), identity);
    assert_eq!(
        fs::read_to_string(user_asset).unwrap(),
        "unrelated user asset"
    );
}

fn run_native_receiver(
    apply_development: bool,
    apply_acceptance: bool,
    apply_delivery: bool,
    apply_repair: bool,
    apply_restart: bool,
) {
    let temp = temp();
    let formal_target = temp.path().join("formal.git");
    let user_asset = temp.path().join("user-owned.txt");
    let excluded = vec![formal_target.clone(), user_asset.clone()];
    run_native_receiver_in(
        apply_development,
        apply_acceptance,
        apply_delivery,
        apply_repair,
        apply_restart,
        temp.path(),
        &formal_target,
        &user_asset,
        &excluded,
    );
}

fn run_native_receiver_in(
    apply_development: bool,
    apply_acceptance: bool,
    apply_delivery: bool,
    apply_repair: bool,
    apply_restart: bool,
    base: &Path,
    formal_target: &Path,
    user_asset: &Path,
    excluded: &[PathBuf],
) {
    assert!(
        !apply_delivery || apply_acceptance,
        "delivery requires the native acceptance stage"
    );
    assert!(
        !apply_repair || (apply_acceptance && apply_delivery),
        "repair requires acceptance and external delivery"
    );
    assert!(
        !apply_restart || (apply_repair && apply_delivery),
        "restart requires repair/delivery"
    );
    let _lock = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    if apply_delivery {
        fs::write(user_asset, "unrelated user asset").unwrap();
    }
    use std::os::unix::fs::MetadataExt;
    let user_identity = apply_delivery.then(|| {
        let m = fs::metadata(user_asset).unwrap();
        (m.dev(), m.ino())
    });
    let mut env = if apply_delivery {
        environment_project(base, formal_target)
    } else {
        environment(base)
    };
    let cwd = env.workspace_root.join("receiver");
    fs::create_dir(&cwd).unwrap();
    let root = task_runtime::prepare(&env, "real-receiver").unwrap();
    let worker_root = apply_development.then(|| task_runtime::prepare(&env, "review").unwrap());
    let reviewer_name = if apply_repair {
        "accept-first"
    } else {
        "accept-native"
    };
    let reviewer_root =
        apply_acceptance.then(|| task_runtime::prepare(&env, reviewer_name).unwrap());
    let repair_root = apply_repair.then(|| task_runtime::prepare(&env, "repair-native").unwrap());
    let final_reviewer_root =
        apply_repair.then(|| task_runtime::prepare(&env, "accept-final").unwrap());
    let mut runtime_roots = vec![root.clone()];
    for runtime in [
        &worker_root,
        &reviewer_root,
        &repair_root,
        &final_reviewer_root,
    ] {
        if let Some(runtime) = runtime {
            runtime_roots.push(runtime.clone());
        }
    }
    let expected_tasks = if apply_repair {
        vec![
            "accept-final".to_string(),
            "accept-first".to_string(),
            "project".to_string(),
            "repair-native".to_string(),
            "review".to_string(),
        ]
    } else {
        vec![
            "accept-native".to_string(),
            "project".to_string(),
            "review".to_string(),
        ]
    };
    let notifications = if apply_repair { 4 } else { 2 };
    let mut artifact_names = vec!["source", "acceptance", "reviewer-checkout"];
    if apply_repair {
        artifact_names.extend(["repair-checkout", "final-reviewer-checkout"]);
    }
    let artifact_roots: Vec<_> = if apply_development {
        artifact_names
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
    let developer_release = root.join("developer-release.json");
    let repair_release = root.join("repair-native-release.json");
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
    if let Some(reviewer_root) = &reviewer_root {
        evidence_paths.extend([
            reviewer_root.join("server.log"),
            reviewer_root.join("submission.json"),
        ]);
    }
    if let Some(repair_root) = &repair_root {
        evidence_paths.extend([
            repair_root.join("server.log"),
            repair_root.join("submission.json"),
        ]);
    }
    if let Some(final_reviewer_root) = &final_reviewer_root {
        evidence_paths.extend([
            final_reviewer_root.join("server.log"),
            final_reviewer_root.join("submission.json"),
        ]);
    }
    if apply_delivery {
        evidence_paths.extend([
            base.join("inventory-before.json"),
            base.join("inventory-after.json"),
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
        if let Some(reviewer_root) = &reviewer_root {
            allow_owned_external_directory(&mut config, reviewer_root);
        }
        if let Some(repair_root) = &repair_root {
            allow_owned_external_directory(&mut config, repair_root);
        }
        if let Some(final_reviewer_root) = &final_reviewer_root {
            allow_owned_external_directory(&mut config, final_reviewer_root);
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
    let mut instruction_text = format!(
        "This is an authorized disposable receiver integration test. Do not delegate or create sessions. Upon receiving the queue notification, use its exact OPENCODE_SESSION_ID to execute the exact ACK command in that notification first. If it succeeds, run {binary} queue readiness --state-dir {queue_path} and require active_wakeup=true. An unacknowledged active delivery intentionally makes readiness false; do not treat that as permission to edit state. Retry only lock-busy errors for at most 15 seconds. Do not fabricate JSON or edit queue/watch files. ACK means receipt only. Then finish your turn. No other task work is authorized."
    );
    if apply_development {
        instruction_text = format!(
            "Authorized disposable coordinator test. Do not delegate or create sessions. Exactly one real development result for task review will arrive. Read the notification and exact bound collector evidence. Use the exact OPENCODE_SESSION_ID from the notification for every herdr-flow command. First execute its exact queue ack command. Then read current queue.json, task review state_version and pending event ID; read the actual collector report referenced by the event's evidence/result, using its output object unchanged. Write an application JSON in cwd with task=review, current state_version, exact event, output from that collector, acceptance={{goal: development assignment completed; independent acceptance still required}}, release_evidence pointing to {release}, next_action={{kind:complete}}. Run {binary} queue task-apply --state-dir {queue_path} --spec YOUR_APPLICATION_FILE. Do not edit queue, guard, reports, or tracked artifacts directly; do not fabricate output or verdicts. Development completion must leave project.approved null. Finally run {binary} queue readiness --state-dir {queue_path}, require active_wakeup=true, and finish. Retry only lock-busy errors for at most15seconds. The driver stops and reaps the developer immediately after production collection; before applying read its factual release receipt at {release}. If this receipt is not present yet, wait up to15seconds for that local file; do not infer release or fabricate it. This is not independent acceptance or delivery. Do not inspect herdr-flow source, operator repositories, or any paths outside the supplied disposable roots. The task-apply schema and command above are complete; write only your application JSON in cwd. No repository search is authorized.",
            release=root.join("developer-release.json").display()
        );
    }
    if apply_acceptance {
        instruction_text = instruction_text.replace("Exactly one real development result for task review will arrive.", "This is a two-stage protocol in the same coordinator session: first development task review, then independent acceptance task accept-native. Each stage has its own notification. Handle task review as follows.").replace("This is not independent acceptance or delivery.", "This first stage does not establish independent acceptance or delivery.");
        instruction_text.push_str(&format!("\nAfter completing development, a second notification for accept-native will arrive in this same session. For that notification ACK its exact delivery first. Read task accept-native, its pending event and registered immutable verdict in queue.json, and the exact referenced collector evidence. Write an application JSON in cwd: task=accept-native, current state_version, exact event, verdict=the registered verdict.id, next_action={{kind: chosen from the actual report conclusion, owner: native coordinator, reason: independent report, condition: existing authorization}}. Map passed to delivery, failed to repair, unable to restore, ambiguous to clarify. Do not invent or rewrite any verdict; do not supply output or acceptance fields for this stage. Run {binary} queue task-apply --state-dir {queue_path} --spec YOUR_APPLICATION_FILE, then queue readiness with that same state-dir and exact session environment. This records a continuation only; do not deliver to any customer or alter artifacts. The earlier development-only schema applies only to task review. Do not read operator repositories."));
    }
    if apply_repair {
        instruction_text = format!(
            "Authorized disposable coordinator test. Do not delegate or create sessions. This is a four-notification protocol in the same coordinator session: review (development), accept-first (acceptance), repair-native (development), accept-final (acceptance). Later notifications reopen this session after you finish a turn. For every notification use the exact OPENCODE_SESSION_ID, execute its exact queue ack first, read the exact task/event and referenced collector report, run queue task-apply, then queue readiness with active_wakeup=true. Application JSON is written in cwd. For development tasks review and repair-native write task=<task>, state_version=<current>, event=<pending event>, output=<collector output unchanged>, acceptance={{goal: development assignment completed; independent acceptance still required}}, release_evidence=<the actual bounded receipt status read from {developer_release} for review or {repair_release} for repair-native>, next_action={{kind:complete}}. Read the corresponding receipt before applying; if absent wait bounded15seconds for the driver to stop/reap and publish it, then require matching collector session and stopped/reaped status. Do not infer or fabricate a stop status. For acceptance tasks accept-first and accept-final write task=<task>, state_version=<current>, event=<pending>, verdict=<the registered immutable verdict.id>, next_action kind from the actual report conclusion: passed to delivery, failed to repair, unable to restore, ambiguous to clarify, with owner=native coordinator, reason=independent report, condition=existing authorization. Never supply acceptance, output, report or a replacement verdict for an acceptance task; never rewrite the verdict. Do not invent or assume a conclusion. Run {binary} queue task-apply --state-dir {queue_path} --spec YOUR_APPLICATION_FILE for each notification. Do not inspect herdr-flow source, operator repositories, sibling roles or any paths outside the supplied disposable roots. Write only the application JSON in cwd; do not fabricate queue/guard/report/verdict state. Finish each turn after readiness; the next notification will reopen the same session.",
            developer_release=developer_release.display(), repair_release=repair_release.display()
        );
    }
    fs::write(&instructions, instruction_text).unwrap();
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
    let mut delivery_expected = None;
    if apply_delivery {
        let source = env.workspace_root.join("source");
        let source_base = git(&source, &["rev-parse", "HEAD"]);
        git(
            base,
            &[
                "clone",
                "--bare",
                source.to_str().unwrap(),
                formal_target.to_str().unwrap(),
            ],
        );
        // setup_in_queue does not impose a source branch name; seed the exact
        // formal ref used by Local delivery regardless of the clone's HEAD.
        git(
            formal_target,
            &["update-ref", "refs/heads/main", source_base.as_str()],
        );
        assert_eq!(
            git(formal_target, &["rev-parse", "refs/heads/main"]),
            source_base
        );
        assert_eq!(
            fs::read_to_string(user_asset).unwrap(),
            "unrelated user asset"
        );
        delivery_expected = Some(source_base);
    }
    let mut queue_child = owned_foreground_watcher(&mut env, "queue", &q);
    let development_result = if let Some(development_cwd) = development_cwd {
        git(&development_cwd, &["config", "user.name", "Native fixture"]);
        git(
            &development_cwd,
            &["config", "user.email", "native@example.invalid"],
        );
        let review_instruction = if apply_repair {
            "This is the deliberately incomplete first review for a bounded repair fixture. Do not complete the requirement: change file to exactly version INCOMPLETE and commit it locally. Do not write an acceptance verdict. No herdr-flow command is needed. Do not search for tools, instructions, sibling directories, coordinator state, or files outside cwd."
        } else {
            "Complete the authorized development assignment: change file to exactly version B and commit locally. Do not write an acceptance verdict. The complete assignment is to write file and commit it here. No herdr-flow command is needed. Do not search for tools, instructions, sibling directories, coordinator state, or files outside cwd."
        };
        let (worker, result) =
            acceptance_live::native(&mut env, &q, &development_cwd, "review", review_instruction);
        assert_ne!(worker["session"], registered["session"]);
        util::atomic_write_json(&developer_release,
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
    if let Some(result) = &development_result {
        let state = queue::load_queue(&q).unwrap();
        assert!(state["tasks"]["project"]["approved"].is_null());
        assert_eq!(state["tasks"]["review"]["pending"], json!([]));
        if apply_repair {
            let receipt = util::read_json(&developer_release).unwrap();
            assert_eq!(receipt["session"], result["session"]);
            assert_eq!(
                state["tasks"]["review"]["last_release"]["evidence"],
                receipt["status"]
            );
        }
        let output = &result["output"];
        let expected_review = if apply_repair {
            "version INCOMPLETE"
        } else {
            "version B"
        };
        assert_eq!(
            git(
                Path::new(output["location"].as_str().unwrap()),
                &["show", &format!("{}:file", output["id"].as_str().unwrap())]
            ),
            expected_review
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
    let mut delivery_artifact = development_result
        .as_ref()
        .map(|result| result["output"].clone());
    if apply_acceptance {
        if apply_repair {
            let development_artifact = &development_result.as_ref().unwrap()["output"];
            let failed_state = native_review_stage(
                &mut env,
                &q,
                &guard,
                &registered,
                ReviewSpec {
                    artifact: development_artifact,
                    reviewer_root: reviewer_root.as_ref().unwrap(),
                    activity: "accept-first",
                    checkout: "reviewer-checkout",
                    instruction: "Independently inspect the exact candidate against the original requirement. This candidate is deliberately incomplete for the bounded repair fixture. Do not edit tracked files or commit. Write .herdr-verdict.json following acceptance_protocol with actual checks, grounded gaps and evidence. Do not assume the conclusion; derive it yourself. Do not inspect sibling directories, coordinator instructions, or operator repositories; all requirement and acceptance protocol inputs are supplied in your packet.",
                    expected_waits: &["review", "accept-first"],
                    expect_pass: false,
                },
            );
            if apply_delivery {
                assert_target_untouched(
                    formal_target,
                    user_asset,
                    user_identity.unwrap(),
                    delivery_expected.as_ref().unwrap(),
                );
            }
            let failed_report =
                failed_state["tasks"]["accept-first"]["verdict"]["envelope"]["report"].clone();
            let failed_verdict = failed_state["tasks"]["accept-first"]["verdict"].clone();
            let failed_report_path = env
                .workspace_root
                .join("reviewer-checkout/.herdr-verdict.json");
            let failed_report_bytes = fs::read(&failed_report_path).unwrap();
            let failed_gaps = failed_report["gaps"].as_array().unwrap().clone();
            assert!(!failed_gaps.is_empty(), "failed acceptance has no gaps");
            assert_eq!(failed_state["tasks"]["accept-first"]["phase"], "blocked");
            assert_eq!(
                failed_state["tasks"]["accept-first"]["lineage"]["input"],
                development_artifact["id"]
            );
            assert_eq!(
                failed_state["tasks"]["project"]["open_gaps"]
                    .as_object()
                    .unwrap()
                    .len(),
                failed_gaps.len()
            );
            for gap in &failed_gaps {
                let gap_id = gap["id"].as_str().unwrap();
                let gap_key = json!(["accept-first", gap_id]).to_string();
                assert_eq!(
                    failed_state["tasks"]["project"]["open_gaps"][&gap_key]["detail"],
                    *gap
                );
                assert_eq!(
                    failed_state["tasks"]["project"]["open_gaps"][gap_key.as_str()]["activity"],
                    "accept-first"
                );
                assert_eq!(
                    failed_state["tasks"]["project"]["open_gaps"][gap_key.as_str()]["gap"],
                    json!(gap_id)
                );
            }
            assert!(failed_state["tasks"]["project"]["approved"].is_null());

            let (repair_output, repair_session) = native_repair_stage(
                &mut env,
                &q,
                &guard,
                &registered,
                development_artifact,
                repair_root.as_ref().unwrap(),
                &failed_report,
            );
            if apply_delivery {
                assert_target_untouched(
                    formal_target,
                    user_asset,
                    user_identity.unwrap(),
                    delivery_expected.as_ref().unwrap(),
                );
            }
            let gap_list = failed_gaps
                .iter()
                .map(|gap| gap["id"].as_str().unwrap())
                .collect::<Vec<_>>()
                .join(", ");
            let final_instruction = format!(
                "Independently inspect the exact repaired candidate against the original requirement. The failed accept-first activity has open gaps {gap_list}. Include a factual resolutions entry for each gap with activity=accept-first, the exact gap id, evidence from the repaired candidate, and a nonempty explanation; resolve every parent.open_gaps entry. Do not edit tracked files or commit. Write .herdr-verdict.json following acceptance_protocol with actual checks and evidence. Derive the conclusion yourself. Do not inspect sibling directories, coordinator instructions, or operator repositories; all requirement and acceptance protocol inputs are supplied in your packet."
            );
            let final_state = native_review_stage(
                &mut env,
                &q,
                &guard,
                &registered,
                ReviewSpec {
                    artifact: &repair_output,
                    reviewer_root: final_reviewer_root.as_ref().unwrap(),
                    activity: "accept-final",
                    checkout: "final-reviewer-checkout",
                    instruction: &final_instruction,
                    expected_waits: &["review", "accept-first", "repair-native", "accept-final"],
                    expect_pass: true,
                },
            );
            if apply_delivery {
                assert_target_untouched(
                    formal_target,
                    user_asset,
                    user_identity.unwrap(),
                    delivery_expected.as_ref().unwrap(),
                );
            }
            assert_eq!(fs::read(&failed_report_path).unwrap(), failed_report_bytes);
            let final_report =
                final_state["tasks"]["accept-final"]["verdict"]["envelope"]["report"].clone();
            assert_eq!(
                final_state["tasks"]["accept-first"]["verdict"], failed_verdict,
                "failed verdict changed after repair"
            );
            assert_eq!(
                final_state["tasks"]["project"]["open_gaps"],
                json!({}),
                "open gaps were not resolved"
            );
            for gap in &failed_gaps {
                let gap_id = gap["id"].as_str().unwrap();
                let resolution = final_report["resolutions"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|resolution| {
                        resolution["activity"] == "accept-first"
                            && resolution["gap"] == json!(gap_id)
                    })
                    .unwrap_or_else(|| panic!("final report lacks resolution for gap {gap_id}"));
                assert!(
                    resolution["evidence"]
                        .as_str()
                        .is_some_and(|value| !value.is_empty()),
                    "gap {gap_id} resolution lacks evidence"
                );
                assert!(
                    resolution["explanation"]
                        .as_str()
                        .is_some_and(|value| !value.is_empty()),
                    "gap {gap_id} resolution lacks explanation"
                );
            }
            let development_id = development_artifact["id"].as_str().unwrap();
            let repair_id = repair_output["id"].as_str().unwrap();
            let parent = &final_state["tasks"]["project"];
            assert_eq!(parent["candidate"], repair_id);
            assert_eq!(parent["approved"]["candidate"], repair_id);
            assert_eq!(parent["approved"]["activity"], "accept-final");
            assert_eq!(
                final_state["tasks"]["accept-first"]["candidate"],
                development_id
            );
            assert_eq!(final_state["tasks"]["accept-final"]["candidate"], repair_id);
            assert_eq!(
                final_state["tasks"]["accept-final"]["lineage"]["input"],
                repair_id
            );
            assert_eq!(
                parent["artifacts"][development_id]["object"],
                *development_artifact
            );
            assert_eq!(parent["artifacts"][repair_id]["object"], repair_output);
            let failed_session = failed_state["tasks"]["accept-first"]["verdict"]["envelope"]
                ["reviewer"]["session"]
                .as_str()
                .unwrap()
                .to_string();
            let final_session = final_state["tasks"]["accept-final"]["verdict"]["envelope"]
                ["reviewer"]["session"]
                .as_str()
                .unwrap()
                .to_string();
            let repair_session = repair_session.as_str().unwrap().to_string();
            let developer_session = development_result.as_ref().unwrap()["session"]
                .as_str()
                .unwrap()
                .to_string();
            let coordinator = registered["session"].as_str().unwrap();
            for session in [
                &failed_session,
                &final_session,
                &repair_session,
                &developer_session,
            ] {
                assert_ne!(session.as_str(), coordinator);
            }
            assert_ne!(failed_session, final_session);
            assert_ne!(failed_session, repair_session);
            assert_ne!(failed_session, developer_session);
            assert_ne!(final_session, repair_session);
            assert_ne!(final_session, developer_session);
            assert_ne!(repair_session, developer_session);
            delivery_artifact = Some(repair_output);
        } else {
            native_acceptance_stage(
                &mut env,
                &q,
                &guard,
                &registered,
                development_result.as_ref().unwrap(),
                reviewer_root.as_ref().unwrap(),
            );
        }
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
        let expected_commands = if apply_repair { 4 } else { 2 };
        for action in ["queue ack", "queue task-apply", "queue readiness"] {
            let count = rows
                .as_array()
                .unwrap()
                .iter()
                .filter(|row| row["info"]["sessionID"] == session_id)
                .flat_map(|row| row["parts"].as_array().into_iter().flatten())
                .filter(|part| {
                    part["type"] == "tool"
                        && part["tool"] == "bash"
                        && part["state"]["status"] == "completed"
                        && part["state"]["input"]["command"]
                            .as_str()
                            .is_some_and(|c| c.contains(action) && c.contains(q.to_str().unwrap()))
                })
                .count();
            assert!(
                count >= expected_commands,
                "all native notifications require completed {action} invocations"
            );
        }
    }
    let inventory = apply_delivery.then(|| {
        super::native_inventory::NativeInventory::new(
            &env,
            &q,
            base,
            excluded.to_vec(),
            runtime_roots.clone(),
            expected_tasks.clone(),
            notifications,
        )
    });
    if apply_restart {
        queue_child.kill().unwrap();
        queue_child.wait().unwrap();
        let artifact = delivery_artifact.as_ref().unwrap();
        let inventory = inventory.expect("restart requires inventory");
        external_supervisor_restart(
            env,
            &q,
            &mut server,
            &mut guard_child,
            RestartSpec {
                target: formal_target,
                user_asset,
                artifact,
                expected: delivery_expected.as_ref().unwrap(),
                user_identity: user_identity.unwrap(),
                inventory,
            },
        );
        return;
    }
    if apply_delivery {
        // The native coordinator has finished both notifications. Stop the queue
        // watcher before the external driver takes the queue lock for delivery.
        queue_child.kill().unwrap();
        queue_child.wait().unwrap();
        let artifact = delivery_artifact.as_ref().unwrap();
        let (inventory, expected_inventory) = external_driver_delivery(
            &mut env,
            &q,
            formal_target,
            user_asset,
            artifact,
            delivery_expected.as_ref().unwrap(),
            user_identity.unwrap(),
            inventory.expect("delivery requires inventory"),
        );
        for child in [&mut server, &mut guard_child] {
            child.wait().unwrap();
        }
        inventory.after(&expected_inventory);
    } else {
        destroy_environment(&env).unwrap();
        for child in [&mut server, &mut guard_child, &mut queue_child] {
            child.wait().unwrap();
        }
    }
    assert!(!env.workspace_root.exists());
    assert!(!env.temp_root.exists());
    assert_eq!(occupied_slots().unwrap(), 0);
}

fn assert_target_untouched(
    target: &Path,
    user_asset: &Path,
    user_identity: (u64, u64),
    expected: &str,
) {
    use std::os::unix::fs::MetadataExt;
    assert_eq!(
        git(target, &["rev-parse", "refs/heads/main"]).as_str(),
        expected
    );
    let metadata = fs::metadata(user_asset).unwrap();
    assert_eq!((metadata.dev(), metadata.ino()), user_identity);
    assert_eq!(
        fs::read_to_string(user_asset).unwrap(),
        "unrelated user asset"
    );
}

fn confirm_delivery(
    env: &mut TaskEnvironment,
    q: &Path,
    target: &Path,
    user_asset: &Path,
    artifact: &J,
    expected: &str,
    user_identity: (u64, u64),
) -> J {
    assert_target_untouched(target, user_asset, user_identity, expected);
    let artifact_id = artifact["id"].as_str().unwrap();
    let state = queue::load_queue(q).unwrap();
    let mut spec = json!({
        "adapter":"git",
        "target":target.display().to_string(),
        "ref":"refs/heads/main",
        "expected":expected,
        "authorization":"deliver"
    });
    spec["state_version"] = state["tasks"]["project"]["state_version"].clone();
    let intent = delivery::prepare(env, q, &spec, &delivery::local::Local).unwrap();
    assert_eq!(intent["artifact"]["id"], json!(artifact_id));
    let intent_id = intent["id"].clone();
    assert_target_untouched(target, user_asset, user_identity, expected);
    let confirmed = delivery::execute(env, &delivery::local::Local).unwrap();
    assert_eq!(confirmed["phase"], "confirmed");
    assert_eq!(confirmed["id"], intent_id);
    assert_eq!(
        confirmed["receipt"]["repository"],
        json!(target.display().to_string())
    );
    assert_eq!(confirmed["receipt"]["ref"], "refs/heads/main");
    assert_eq!(confirmed["receipt"]["commit"], json!(artifact_id));
    assert_eq!(
        git(target, &["rev-parse", "refs/heads/main"]).as_str(),
        artifact_id
    );
    assert_eq!(
        fs::read_to_string(user_asset).unwrap(),
        "unrelated user asset"
    );
    confirmed
}

fn cli_output(args: &[&str]) -> std::process::Output {
    use std::io::{Read, Seek, SeekFrom};
    fn read_bounded(file: &mut std::fs::File) -> Vec<u8> {
        let mut bytes = Vec::new();
        let _ = file.seek(SeekFrom::Start(0));
        let _ = file.take(1024 * 1024).read_to_end(&mut bytes);
        bytes
    }
    let mut stdout_file = tempfile::tempfile().unwrap();
    let mut stderr_file = tempfile::tempfile().unwrap();
    let mut child = CliChild {
        child: Some(
            std::process::Command::new(env!("CARGO_BIN_EXE_herdr-flow"))
                .args(args)
                .stdout(std::process::Stdio::from(stdout_file.try_clone().unwrap()))
                .stderr(std::process::Stdio::from(stderr_file.try_clone().unwrap()))
                .spawn()
                .unwrap(),
        ),
    };
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let status = child.child.as_mut().unwrap().try_wait().unwrap();
        if let Some(status) = status {
            let _ = child.child.take();
            return std::process::Output {
                status,
                stdout: read_bounded(&mut stdout_file),
                stderr: read_bounded(&mut stderr_file),
            };
        }
        if Instant::now() >= deadline {
            if let Some(mut process) = child.child.take() {
                let _ = process.kill();
                let _ = process.wait();
            }
            panic!(
                "delivery CLI timed out; stdout={:?}; stderr={:?}",
                String::from_utf8_lossy(&read_bounded(&mut stdout_file)),
                String::from_utf8_lossy(&read_bounded(&mut stderr_file))
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

struct CliChild {
    child: Option<std::process::Child>,
}
impl Drop for CliChild {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn wait_bounded(child: &mut std::process::Child, label: &str) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Ok(None) => {
                reap_after_kill(child, label);
                panic!("{label} did not exit within 15s");
            }
            Err(error) => {
                reap_after_kill(child, label);
                panic!("{label} wait failed: {error}");
            }
        }
    }
}

fn reap_after_kill(child: &mut std::process::Child, label: &str) {
    let _ = child.kill();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Ok(None) => {
                eprintln!("{label} could not be reaped within 5s after kill");
                return;
            }
            Err(error) => {
                eprintln!("{label} reap failed: {error}");
                return;
            }
        }
    }
}

struct ChildReaper<'a> {
    server: &'a mut std::process::Child,
    guard: &'a mut std::process::Child,
    armed: bool,
}
impl<'a> ChildReaper<'a> {
    fn new(server: &'a mut std::process::Child, guard: &'a mut std::process::Child) -> Self {
        Self {
            server,
            guard,
            armed: true,
        }
    }
    fn kill(&mut self) {
        if !self.armed {
            return;
        }
        self.armed = false;
        reap_after_kill(self.server, "native server");
        reap_after_kill(self.guard, "guard watcher");
    }
    fn wait(mut self) {
        wait_bounded(self.server, "native server");
        wait_bounded(self.guard, "guard watcher");
        self.armed = false;
    }
}
impl Drop for ChildReaper<'_> {
    fn drop(&mut self) {
        self.kill();
    }
}

struct RestartSpec<'a> {
    inventory: super::native_inventory::NativeInventory,
    target: &'a Path,
    user_asset: &'a Path,
    artifact: &'a J,
    expected: &'a str,
    user_identity: (u64, u64),
}

fn external_supervisor_restart(
    mut env: super::OwnedEnvironment,
    q: &Path,
    server: &mut std::process::Child,
    guard_child: &mut std::process::Child,
    spec: RestartSpec<'_>,
) {
    let RestartSpec {
        inventory,
        target,
        user_asset,
        artifact,
        expected,
        user_identity,
    } = spec;
    let workspace = env.workspace_root.clone();
    let temporary = env.temp_root.clone();
    let state_file = env.state_file().to_path_buf();
    let mut custody;
    let mut reaper = ChildReaper::new(server, guard_child);
    let confirmed = confirm_delivery(
        &mut env,
        q,
        target,
        user_asset,
        artifact,
        expected,
        user_identity,
    );
    let artifact_id = artifact["id"].as_str().unwrap();
    let intent_id = confirmed["id"].clone();
    let receipt = confirmed["receipt"].clone();
    let expected_inventory = inventory.before(&confirmed);
    custody = env.into_handoff();

    let reconcile = cli_output(&["delivery", "reconcile", "--task", "project"]);
    if !reconcile.status.success() {
        reaper.kill();
        custody.cleanup();
        panic!(
            "fresh reconcile failed: {}",
            String::from_utf8_lossy(&reconcile.stderr)
        );
    }
    let reconciled: J = serde_json::from_slice(&reconcile.stdout)
        .unwrap_or_else(|error| panic!("reconcile output invalid: {error}"));
    assert_eq!(reconciled["phase"], "confirmed");
    assert_eq!(reconciled["id"], intent_id);
    assert_eq!(reconciled["receipt"], receipt);
    assert_target_untouched(target, user_asset, user_identity, artifact_id);

    let finish = cli_output(&["delivery", "finish", "--task", "project"]);
    if !finish.status.success() {
        reaper.kill();
        custody.cleanup();
        panic!(
            "fresh finish failed: {}",
            String::from_utf8_lossy(&finish.stderr)
        );
    }
    reaper.wait();
    assert!(!workspace.exists());
    assert!(!temporary.exists());
    assert!(!state_file.exists());
    assert!(!is_registered("project").unwrap());
    assert!(!cleanup_state_path("project").unwrap().exists());
    assert_eq!(occupied_slots().unwrap(), 0);
    assert!(!q.exists());
    assert_target_untouched(target, user_asset, user_identity, artifact_id);
    assert_eq!(git(target, &["show", "refs/heads/main:file"]), "version B");
    inventory.after(&expected_inventory);
    custody.disarm();
}

// Trusted, unmarked external driver. The native coordinator only approved the
// continuation; this process owns the environment and the Local executor.
fn external_driver_delivery(
    env: &mut TaskEnvironment,
    q: &Path,
    target: &Path,
    user_asset: &Path,
    artifact: &J,
    expected: &str,
    user_identity: (u64, u64),
    inventory: super::native_inventory::NativeInventory,
) -> (super::native_inventory::NativeInventory, J) {
    use std::os::unix::fs::MetadataExt;
    let m = fs::metadata(user_asset).unwrap();
    assert_eq!((m.dev(), m.ino()), user_identity);
    let artifact_id = artifact["id"].as_str().unwrap();
    assert_eq!(
        git(target, &["rev-parse", "refs/heads/main"]).as_str(),
        expected
    );
    assert_eq!(
        fs::read_to_string(user_asset).unwrap(),
        "unrelated user asset"
    );
    let state = queue::load_queue(q).unwrap();
    let mut spec = json!({
        "adapter":"git",
        "target":target.display().to_string(),
        "ref":"refs/heads/main",
        "expected":expected,
        "authorization":"deliver"
    });
    assert_eq!(
        state["tasks"]["project"]["requirements"]["1"]["basis"]["endpoint"],
        json!(target)
    );
    spec["state_version"] = state["tasks"]["project"]["state_version"].clone();
    let intent = delivery::prepare(env, q, &spec, &delivery::local::Local).unwrap();
    assert_eq!(intent["artifact"]["id"], json!(artifact_id));
    // Preparation is not delivery: the exact seeded target must be unchanged.
    assert_eq!(
        git(target, &["rev-parse", "refs/heads/main"]).as_str(),
        expected
    );
    assert_eq!(
        fs::read_to_string(user_asset).unwrap(),
        "unrelated user asset"
    );

    let confirmed = delivery::execute(env, &delivery::local::Local).unwrap();
    assert_eq!(confirmed["phase"], "confirmed");
    assert_eq!(
        confirmed["receipt"]["repository"],
        json!(target.display().to_string())
    );
    assert_eq!(confirmed["receipt"]["ref"], "refs/heads/main");
    assert_eq!(confirmed["receipt"]["commit"], json!(artifact_id));
    assert_eq!(
        git(target, &["rev-parse", "refs/heads/main"]).as_str(),
        artifact_id
    );
    assert_eq!(
        fs::read_to_string(user_asset).unwrap(),
        "unrelated user asset"
    );
    let expected_inventory = inventory.before(&confirmed);

    delivery::finish(env).unwrap();
    let m = fs::metadata(user_asset).unwrap();
    assert_eq!((m.dev(), m.ino()), user_identity);
    assert_eq!(git(target, &["show", "refs/heads/main:file"]), "version B");
    assert!(!env.workspace_root.exists());
    assert!(!env.temp_root.exists());
    assert!(!env.state_file().exists());
    assert!(!is_registered("project").unwrap());
    assert!(!cleanup_state_path("project").unwrap().exists());
    assert_eq!(occupied_slots().unwrap(), 0);
    assert!(!q.exists());
    assert_eq!(
        git(target, &["rev-parse", "refs/heads/main"]).as_str(),
        artifact_id
    );
    assert_eq!(
        fs::read_to_string(user_asset).unwrap(),
        "unrelated user asset"
    );
    (inventory, expected_inventory)
}

fn wait_for_stage(q: &Path, guard: &Path, coordinator: &J, wait: &str) -> J {
    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        let state = queue::load_queue(q).unwrap();
        let matches: Vec<_> = state["events"]
            .as_object()
            .unwrap()
            .iter()
            .filter(|(_, event)| event["wait"] == wait)
            .collect();
        assert!(matches.len() <= 1, "duplicate {wait} event");
        if let Some((id, event)) = matches.first() {
            let receipt = state["deliveries"]
                .as_object()
                .unwrap()
                .iter()
                .find(|(_, delivery)| {
                    delivery["events"] == json!([id])
                        && delivery["phase"] == "acknowledged"
                        && delivery["session"] == coordinator["session"]
                });
            if let Some((delivery, _)) = receipt {
                let readiness = util::read_json(&q.join("readiness.json")).unwrap_or(J::Null);
                let submission = util::read_json(&guard.join("submission.json")).unwrap_or(J::Null);
                assert_ne!(
                    submission["native_status"], "blocked",
                    "native approval blocked {wait}"
                );
                if event["applied"] == true
                    && readiness["active_wakeup"] == true
                    && readiness["last_confirmed_receipt"]["delivery"] == *delivery
                    && readiness["last_confirmed_receipt"]["session"] == coordinator["session"]
                    && submission["phase"] == "complete"
                {
                    return state;
                }
            }
        }
        assert!(
            Instant::now() < deadline,
            "native {wait} application deadline"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn assert_stage_shape(state: &J, waits: &[&str], coordinator: &J) {
    assert_eq!(state["events"].as_object().unwrap().len(), waits.len());
    assert_eq!(state["deliveries"].as_object().unwrap().len(), waits.len());
    for wait in waits {
        let matches: Vec<_> = state["events"]
            .as_object()
            .unwrap()
            .iter()
            .filter(|(_, event)| event["wait"] == *wait)
            .collect();
        assert_eq!(matches.len(), 1, "exactly one event for {wait}");
        let (id, event) = matches[0];
        assert_eq!(event["acknowledged"], true);
        assert_eq!(event["applied"], true);
        let deliveries: Vec<_> = state["deliveries"]
            .as_object()
            .unwrap()
            .values()
            .filter(|delivery| delivery["events"] == json!([id]))
            .collect();
        assert_eq!(deliveries.len(), 1, "exactly one delivery for {wait}");
        assert_eq!(deliveries[0]["session"], coordinator["session"]);
        assert_eq!(deliveries[0]["phase"], "acknowledged");
    }
}

struct ReviewSpec<'a> {
    artifact: &'a J,
    reviewer_root: &'a Path,
    activity: &'a str,
    checkout: &'a str,
    instruction: &'a str,
    expected_waits: &'a [&'a str],
    expect_pass: bool,
}

fn native_review_stage(
    env: &mut TaskEnvironment,
    q: &Path,
    guard: &Path,
    coordinator: &J,
    spec: ReviewSpec<'_>,
) -> J {
    let ReviewSpec {
        artifact,
        reviewer_root,
        activity: activity_name,
        checkout: checkout_name,
        instruction: reviewer_instruction,
        expected_waits,
        expect_pass,
    } = spec;
    acceptance_live::change(
        q,
        json!({"kind":"candidate","candidate":artifact["id"],"reason":"real independent acceptance"}),
    );
    let mut activity = acceptance_live::activity(activity_name, "acceptance", &artifact["id"]);
    activity["kind"] = json!("activity");
    acceptance_live::change(q, activity);
    let cwd = acceptance_live::clone_snapshot(env, artifact, checkout_name);
    let (reviewer, collected) =
        acceptance_live::native(env, q, &cwd, activity_name, reviewer_instruction);
    assert_ne!(reviewer["session"], coordinator["session"]);
    let actual_report = util::read_json(&cwd.join(".herdr-verdict.json")).unwrap();
    let state = wait_for_stage(q, guard, coordinator, activity_name);
    assert_stage_shape(&state, expected_waits, coordinator);
    let task = &state["tasks"][activity_name];
    assert_eq!(task["pending"], json!([]));
    let verdict = &task["verdict"];
    let event = state["events"]
        .as_object()
        .unwrap()
        .values()
        .find(|event| event["wait"] == activity_name)
        .unwrap();
    assert_eq!(event["binding"], collected["context"]["binding"]);
    assert_eq!(verdict["envelope"]["binding"], event["binding"]);
    assert_eq!(
        verdict["envelope"]["reviewer"]["session"],
        reviewer["session"]
    );
    assert_eq!(verdict["envelope"]["reviewer"]["adapter"], "opencode");
    assert_eq!(verdict["envelope"]["report"], actual_report);
    assert_eq!(
        util::read_json(&cwd.join(".herdr-verdict.json")).unwrap(),
        actual_report
    );
    assert_eq!(event["outcome"], "complete");
    assert_eq!(
        event["evidence"],
        json!(q.join(format!("results/{activity_name}.json")))
    );
    let signal = util::read_json(Path::new(event["evidence"].as_str().unwrap())).unwrap();
    assert_eq!(signal["wait"], activity_name);
    assert_eq!(signal["outcome"], "complete");
    assert_eq!(
        signal["evidence"],
        json!(reviewer_root.join("submissions").join(format!(
            "{}.json",
            collected["message_id"].as_str().unwrap()
        )))
    );
    let report = util::read_json(Path::new(signal["evidence"].as_str().unwrap())).unwrap();
    assert_eq!(report["session"], reviewer["session"]);
    let parent = &state["tasks"]["project"];
    assert_eq!(parent["candidate"], artifact["id"]);
    assert_eq!(parent["delivery_target"], artifact["id"]);
    assert_eq!(task["candidate"], artifact["id"]);
    assert_eq!(parent["verdicts"][activity_name], *verdict);
    assert_eq!(parent["continuation"]["id"], activity_name);
    assert_eq!(parent["continuation"]["phase"], "planned");
    assert_eq!(parent["continuation"]["verdict"], verdict["id"]);
    assert_eq!(parent["continuation"]["binding"], event["binding"]);
    if expect_pass {
        assert_eq!(actual_report["conclusion"], "passed");
        assert_eq!(parent["approved"]["candidate"], artifact["id"]);
        assert_eq!(parent["approved"]["activity"], activity_name);
        assert_eq!(parent["approved"]["round"], task["lineage"]["round"]);
        assert_eq!(parent["approved"]["verdict"], verdict["id"]);
        assert_eq!(task["phase"], "completed");
        assert_eq!(parent["continuation"]["action"]["kind"], "delivery");
    } else {
        assert_eq!(actual_report["conclusion"], "failed");
        assert!(parent["approved"].is_null());
        assert_eq!(parent["continuation"]["action"]["kind"], "repair");
    }
    state
}

fn native_repair_stage(
    env: &mut TaskEnvironment,
    q: &Path,
    guard: &Path,
    coordinator: &J,
    old_artifact: &J,
    repair_root: &Path,
    failed_report: &J,
) -> (J, J) {
    let gaps = failed_report["gaps"].as_array().unwrap();
    assert!(!gaps.is_empty(), "failed acceptance has no gaps");
    let resolution = gaps
        .iter()
        .map(|gap| {
            format!(
                "repair accept-first gap {}: {}",
                gap["id"].as_str().unwrap(),
                gap["evidence"].as_str().unwrap_or("grounded failed review")
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    let activity = acceptance_live::activity("repair-native", "development", &old_artifact["id"]);
    let spec = json!({
        "kind":"continue",
        "intent":"accept-first",
        "authorization":"develop",
        "action":"development",
        "resolution":resolution,
        "activity":activity
    });
    acceptance_live::change(q, spec);
    let cwd = acceptance_live::clone_snapshot(env, old_artifact, "repair-checkout");
    let (repair, collected) = acceptance_live::native(
        env,
        q,
        &cwd,
        "repair-native",
        "Repair the recorded failed acceptance gap against the original requirement. Change file to exactly version B and commit locally. Do not write an acceptance verdict or commit acceptance metadata. Stay within this checkout; do not inspect sibling directories, coordinator instructions, or operator repositories.",
    );
    assert_ne!(repair["session"], coordinator["session"]);
    util::atomic_write_json(
        &q.parent().unwrap().join("repair-native-release.json"),
        &json!({
            "session":repair["session"],
            "runtime":repair_root,
            "status":"server stopped and reaped after terminal collection"
        }),
    )
    .unwrap();
    let state = wait_for_stage(q, guard, coordinator, "repair-native");
    let receipt = util::read_json(&q.parent().unwrap().join("repair-native-release.json")).unwrap();
    assert_eq!(receipt["session"], repair["session"]);
    assert_eq!(receipt["session"], collected["session"]);
    assert_eq!(
        state["tasks"]["repair-native"]["last_release"]["evidence"],
        receipt["status"]
    );
    assert_stage_shape(
        &state,
        &["review", "accept-first", "repair-native"],
        coordinator,
    );
    let task = &state["tasks"]["repair-native"];
    assert_eq!(task["pending"], json!([]));
    assert_eq!(task["phase"], "completed");
    assert_eq!(task["lineage"]["input"], old_artifact["id"]);
    let event = state["events"]
        .as_object()
        .unwrap()
        .values()
        .find(|event| event["wait"] == "repair-native")
        .unwrap();
    assert_eq!(event["binding"], collected["context"]["binding"]);
    assert_eq!(
        event["evidence"],
        json!(q.join("results/repair-native.json"))
    );
    let signal = util::read_json(Path::new(event["evidence"].as_str().unwrap())).unwrap();
    assert_eq!(signal["wait"], "repair-native");
    assert_eq!(signal["outcome"], "complete");
    assert_eq!(
        signal["evidence"],
        json!(repair_root.join("submissions").join(format!(
            "{}.json",
            collected["message_id"].as_str().unwrap()
        )))
    );
    let report = util::read_json(Path::new(signal["evidence"].as_str().unwrap())).unwrap();
    assert_eq!(report["session"], repair["session"]);
    let output = collected["output"].clone();
    assert_ne!(output["id"], old_artifact["id"]);
    assert_eq!(report["output"], output);
    assert_eq!(
        git(
            Path::new(output["location"].as_str().unwrap()),
            &["show", &format!("{}:file", output["id"].as_str().unwrap())]
        ),
        "version B"
    );
    assert_eq!(
        state["tasks"]["project"]["artifacts"][output["id"].as_str().unwrap()]["object"],
        output
    );
    assert_eq!(
        state["tasks"]["project"]["activities"]["repair-native"]["output"],
        output["id"]
    );
    assert!(state["tasks"]["project"]["approved"].is_null());
    assert_eq!(state["tasks"]["project"]["candidate"], old_artifact["id"]);
    (output, repair["session"].clone())
}

// Driver registers work and collects native evidence; only the coordinator ACKs/applies.
fn native_acceptance_stage(
    env: &mut TaskEnvironment,
    q: &Path,
    guard: &Path,
    coordinator: &J,
    development: &J,
    reviewer_root: &Path,
) {
    let artifact = &development["output"];
    acceptance_live::change(
        q,
        json!({"kind":"candidate","candidate":artifact["id"],"reason":"real independent acceptance"}),
    );
    let mut activity = acceptance_live::activity("accept-native", "acceptance", &artifact["id"]);
    activity["kind"] = json!("activity");
    acceptance_live::change(q, activity);
    let cwd = acceptance_live::clone_snapshot(env, artifact, "reviewer-checkout");
    let (reviewer, collected) = acceptance_live::native(env, q, &cwd, "accept-native", "Independently inspect the exact candidate against the original requirement. Do not edit tracked files or commit. Write .herdr-verdict.json following acceptance_protocol with actual checks and grounded evidence. Derive the conclusion yourself. Do not inspect sibling directories, coordinator instructions, or operator repositories; all requirement and acceptance protocol inputs are supplied in your packet.");
    assert_ne!(reviewer["session"], development["session"]);
    assert_ne!(reviewer["session"], coordinator["session"]);
    let actual_report = util::read_json(&cwd.join(".herdr-verdict.json")).unwrap();
    let deadline = Instant::now() + Duration::from_secs(180);
    let state = loop {
        let state = queue::load_queue(q).unwrap();
        let events: Vec<_> = state["events"]
            .as_object()
            .unwrap()
            .iter()
            .filter(|(_, e)| e["wait"] == "accept-native")
            .collect();
        assert!(events.len() <= 1, "duplicate acceptance event");
        if let Some((id, event)) = events.first() {
            let receipt = state["deliveries"]
                .as_object()
                .unwrap()
                .iter()
                .find(|(_, d)| {
                    d["events"] == json!([id])
                        && d["phase"] == "acknowledged"
                        && d["session"] == coordinator["session"]
                });
            if let Some((delivery, _)) = receipt {
                let readiness = util::read_json(&q.join("readiness.json")).unwrap_or(J::Null);
                let submission = util::read_json(&guard.join("submission.json")).unwrap_or(J::Null);
                assert_ne!(
                    submission["native_status"], "blocked",
                    "native approval blocked acceptance"
                );
                if event["applied"] == true
                    && readiness["active_wakeup"] == true
                    && readiness["last_confirmed_receipt"]["delivery"] == *delivery
                    && readiness["last_confirmed_receipt"]["session"] == coordinator["session"]
                    && submission["phase"] == "complete"
                {
                    break state;
                }
            }
        }
        assert!(
            Instant::now() < deadline,
            "native acceptance application deadline"
        );
        std::thread::sleep(Duration::from_millis(200));
    };
    assert_eq!(state["events"].as_object().unwrap().len(), 2);
    assert_eq!(state["deliveries"].as_object().unwrap().len(), 2);
    for wait in ["review", "accept-native"] {
        let matches: Vec<_> = state["events"]
            .as_object()
            .unwrap()
            .iter()
            .filter(|(_, e)| e["wait"] == wait)
            .collect();
        assert_eq!(matches.len(), 1);
        let (id, event) = matches[0];
        assert_eq!(event["acknowledged"], true);
        assert_eq!(event["applied"], true);
        let deliveries: Vec<_> = state["deliveries"]
            .as_object()
            .unwrap()
            .values()
            .filter(|d| d["events"] == json!([id]))
            .collect();
        assert_eq!(deliveries.len(), 1);
        assert_eq!(deliveries[0]["session"], coordinator["session"]);
        assert_eq!(deliveries[0]["phase"], "acknowledged");
    }
    let task = &state["tasks"]["accept-native"];
    assert_eq!(task["pending"], json!([]));
    let verdict = &task["verdict"];
    let event = state["events"]
        .as_object()
        .unwrap()
        .values()
        .find(|e| e["wait"] == "accept-native")
        .unwrap();
    assert_eq!(event["binding"], collected["context"]["binding"]);
    assert_eq!(verdict["envelope"]["binding"], event["binding"]);
    assert_eq!(
        verdict["envelope"]["reviewer"]["session"],
        reviewer["session"]
    );
    assert_eq!(verdict["envelope"]["report"], actual_report);
    assert_eq!(verdict["envelope"]["reviewer"]["adapter"], "opencode");
    assert_eq!(
        util::read_json(&cwd.join(".herdr-verdict.json")).unwrap(),
        actual_report
    );
    assert_eq!(event["outcome"], "complete");
    assert_eq!(event["acknowledged"], true);
    assert_eq!(
        event["evidence"],
        json!(q.join("results/accept-native.json"))
    );
    let signal = util::read_json(Path::new(event["evidence"].as_str().unwrap())).unwrap();
    assert_eq!(signal["wait"], "accept-native");
    assert_eq!(signal["outcome"], "complete");
    assert_eq!(
        signal["evidence"],
        json!(reviewer_root.join("submissions").join(format!(
            "{}.json",
            collected["message_id"].as_str().unwrap()
        )))
    );
    let report = util::read_json(Path::new(signal["evidence"].as_str().unwrap())).unwrap();
    assert_eq!(report["session"], reviewer["session"]);
    let parent = &state["tasks"]["project"];
    assert_eq!(parent["candidate"], artifact["id"]);
    assert_eq!(parent["delivery_target"], artifact["id"]);
    assert_eq!(task["candidate"], artifact["id"]);
    assert_eq!(parent["verdicts"]["accept-native"], *verdict);
    assert_eq!(parent["continuation"]["id"], "accept-native");
    assert_eq!(parent["continuation"]["phase"], "planned");
    assert_eq!(parent["continuation"]["verdict"], verdict["id"]);
    assert_eq!(parent["continuation"]["binding"], event["binding"]);
    if actual_report["conclusion"] == "passed" {
        assert_eq!(parent["approved"]["candidate"], artifact["id"]);
        assert_eq!(parent["approved"]["activity"], "accept-native");
        assert_eq!(parent["approved"]["round"], task["lineage"]["round"]);
        assert_eq!(parent["approved"]["verdict"], verdict["id"]);
        assert_eq!(task["phase"], "completed");
        assert_eq!(parent["continuation"]["action"]["kind"], "delivery");
    } else {
        assert!(parent["approved"].is_null());
    }
    assert_eq!(
        actual_report["conclusion"], "passed",
        "independent reviewer did not accept the candidate; no pass manufactured"
    );
}
