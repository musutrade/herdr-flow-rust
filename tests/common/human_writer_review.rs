use super::*;
use std::time::{Duration, Instant};

#[test]
fn missing_paths_and_corrupt_records_are_fences_without_breaking_observation() {
    let (f, service) = Fixture::with_server(0, "idle");
    f.request();
    check_workspace(&f.state(), &f.temp.path().join("future/workspace")).unwrap();
    let error = check_workspace(&f.state(), &f.checkout().join("missing/child")).unwrap_err();
    assert!(error.message.contains("human handoff"));
    let pristine = f.state();
    let mut cases = vec![];
    let mut state = pristine.clone();
    state["tasks"] = json!([]);
    cases.push(state);
    let mut state = pristine.clone();
    state["tasks"]["parent"]["environment"] = J::Null;
    cases.push(state);
    let mut state = pristine.clone();
    state["tasks"]["other"] = json!({"human_writer":{"phase":"released"}});
    cases.push(state);
    let mut state = pristine.clone();
    state["tasks"]["parent"][KEY]["padding"] = json!("x".repeat(LIMIT));
    cases.push(state);
    for state in cases {
        let error = check_workspace(&state, &f.temp.path().join("unrelated")).unwrap_err();
        assert!(error.message.contains("human handoff"), "{error}");
        util::atomic_write_json(&f.owner.queue_root.join("queue.json"), &state).unwrap();
        assert!(!automatic_input_allowed(&f.root()).unwrap());
        assert!(admit_native(&f.root(), &json!({"method":"turn/start"})).is_err());
        admit_native(&f.root(), &json!({"method":"thread/read"})).unwrap();
        admit_native(&f.root(), &json!({"method":"tool/respond"})).unwrap();
    }
    service.finish();
}

#[test]
fn missing_or_malformed_runtime_blocks_writes_but_explicit_legacy_remains_usable() {
    let root = util::fs::test_tempdir();
    let write = json!({"method":"turn/start"});
    assert!(admit_native(root.path(), &write).is_err());
    assert!(!automatic_input_allowed(root.path()).unwrap());
    admit_native(root.path(), &json!({"method":"turn/interrupt"})).unwrap();
    util::atomic_write_json(&root.path().join("runtime.json"), &json!({"cwd":"/legacy"})).unwrap();
    admit_native(root.path(), &write).unwrap();
    util::atomic_write_json(
        &root.path().join("runtime.json"),
        &json!({"lifecycle_owner":"corrupt"}),
    )
    .unwrap();
    assert!(admit_native(root.path(), &write).is_err());
    let isolated = root.path().join("isolated");
    util::ensure_private_dir(&isolated, true).unwrap();
    util::protected::runtime_namespace(&isolated).unwrap();
    util::atomic_write_json(
        &isolated.join("runtime.json"),
        &json!({"cwd":"/lost-owner"}),
    )
    .unwrap();
    assert!(!automatic_input_allowed(&isolated).unwrap());
}

#[test]
fn oversized_history_can_be_inspected_verified_and_reacquired() {
    let (f, service) = Fixture::with_server(5, "idle");
    f.request();
    f.acquire();
    f.give_back();
    let incident = f.incident();
    let mut result = verdict(&incident, "needs_human");
    result["facts"] = json!("证据".repeat(10_000));
    assert!(serde_json::to_vec(&result).unwrap().len() < 65536);
    let mut state = f.state();
    let old = json!({"return":state["tasks"]["parent"][KEY]["return"],
        "verification":{"incident":"old-incident","turn":"old-turn","agent":{},
            "model_result":result,"result":result}});
    state["tasks"]["parent"][KEY]["verifications"] = json!(vec![old; 64]);
    let oversized = state["tasks"]["parent"][KEY].clone();
    assert!(serde_json::to_vec(&oversized).unwrap().len() > LIMIT);
    f.write_state(&mut state);
    let view = status(&f.root(), "parent").unwrap();
    assert_eq!(view["warning"]["kind"], "oversized_handoff");
    assert!(serde_json::to_vec(&view).unwrap().len() < LIMIT);
    assert_eq!(f.state()["tasks"]["parent"][KEY], oversized);
    assert_eq!(
        finish_verification(&f.context(), &incident, &result).unwrap(),
        result
    );
    let verified = f.state()["tasks"]["parent"][KEY].clone();
    assert!(serde_json::to_vec(&verified).unwrap().len() < LIMIT);
    assert_eq!(verified["verifications"].as_array().unwrap().len(), 65);
    assert_eq!(
        verified["verifications"][0]["verification"]["turn"],
        "old-turn"
    );
    assert_eq!(
        verified["verifications"][64]["verification"]["result"]["disposition"],
        "needs_human"
    );
    assert_eq!(
        finish_verification(&f.context(), &incident, &result).unwrap(),
        result
    );
    assert_eq!(f.state()["tasks"]["parent"][KEY], verified);
    let acquired = f.acquire();
    assert_eq!(acquired["phase"], "human");
    assert!(acquired["return"].is_null());
    assert!(acquired["verification"].is_null());
    assert_eq!(acquired["verifications"], verified["verifications"]);
    assert!(finish_verification(&f.context(), &incident, &result).is_err());
    let returned = f.give_back();
    assert_ne!(returned["return"]["id"], incident["fault"]["id"]);
    let fresh = f.incident();
    finish_verification(&f.context(), &fresh, &verdict(&fresh, "recovered")).unwrap();
    assert_eq!(f.state()["tasks"]["parent"][KEY]["phase"], "released");
    assert!(automatic_input_allowed(&f.root()).unwrap());
    service.finish();
}

#[test]
fn history_prunes_by_serialized_bytes_and_retains_a_receipt() {
    let mut record = json!({"verification":{"incident":"current","turn":"current-turn",
        "model_result":{"facts":"x".repeat(60_000)},"result":{"facts":"y".repeat(60_000)}}});
    let entries = (0..400).map(|i| json!({"return":{"id":format!("return-{i}"),"note":"文".repeat(1000)},
        "verification":{"incident":format!("incident-{i}"),"turn":format!("turn-{i}"),
            "model_result":{"facts":"x".repeat(2000),"actions":"a".repeat(2000),"next_step":"n".repeat(2000)},
            "result":{"disposition":"needs_human","facts":"x".repeat(2000),"actions":"a".repeat(2000),"next_step":"n".repeat(2000)}}})).collect::<Vec<_>>();
    record["verifications"] = json!(entries);
    history::fit(&mut record).unwrap();
    assert!(serde_json::to_vec(&record).unwrap().len() < LIMIT);
    let retained = record["verifications"].as_array().unwrap();
    assert!(retained.len() < 400);
    assert_eq!(
        retained.last().unwrap()["verification"]["incident"],
        "incident-399"
    );
    assert_eq!(
        record["verification_history_pruned"]["count"]
            .as_u64()
            .unwrap() as usize
            + retained.len(),
        400
    );
    assert_eq!(
        record["verification_history_pruned"]["sha256"]
            .as_str()
            .unwrap()
            .len(),
        64
    );
    assert_eq!(
        record["verification"]["model_result"]["facts"]
            .as_str()
            .unwrap()
            .len(),
        60_000
    );
}

#[test]
fn abandon_all_active_phases_allows_only_failure_without_releasing_writes() {
    for phase in ["draining", "human", "verifying", "blocked"] {
        abandon_phase(phase);
    }
}

fn abandon_phase(phase: &str) {
    let count = match phase {
        "draining" => 0,
        "human" => 1,
        _ => 2,
    };
    let (f, service) = Fixture::with_server(count, "idle");
    f.request();
    if phase != "draining" {
        f.acquire();
    }
    let mut incident = J::Null;
    if count == 2 {
        f.give_back();
        incident = f.incident();
    }
    if phase == "blocked" {
        finish_verification(&f.context(), &incident, &verdict(&incident, "needs_human")).unwrap();
    }
    let before = f.state()["tasks"]["parent"].clone();
    assert_eq!(before[KEY]["phase"], phase);
    util::atomic_write_json(
        &f.root().join("recovery-observer/agent.json"),
        &json!({"phase":"unavailable"}),
    )
    .unwrap();
    assert!(f
        .action("human-abandon", json!({"reason":"unrecoverable"}))
        .is_err());
    let spec = json!({"reason":"unrecoverable","acknowledge_cleanup":true});
    let abandoned = f.action("human-abandon", spec.clone()).unwrap();
    assert_eq!(abandoned["phase"], "abandoned");
    assert_eq!(abandoned["writer"], "none");
    assert_eq!(abandoned["abandon"]["from_phase"], phase);
    assert_eq!(f.action("human-abandon", spec).unwrap(), abandoned);
    assert_eq!(f.state()["tasks"]["parent"]["acceptance_round"], 3);
    assert_eq!(f.state()["tasks"]["parent"]["verdicts"], before["verdicts"]);
    assert!(f.state()["tasks"]["parent"]["candidate"].is_null());
    assert!(!automatic_input_allowed(&f.root()).unwrap());
    assert!(f.action("human-acquire", json!({})).is_err());
    assert!(f.action("human-return", json!({"note":"late"})).is_err());
    if !incident.is_null() {
        assert!(
            finish_verification(&f.context(), &incident, &verdict(&incident, "needs_human"))
                .is_err()
        );
    }
    assert_abandoned_terminal(&f);
    service.finish();
}

fn assert_abandoned_terminal(f: &Fixture) {
    let env = f.active();
    let mut state = f.state();
    let mut spec = json!({"outcome":"success","reason":"abandon","state_version":state["tasks"]["parent"]["state_version"]});
    assert!(
        crate::progress::terminal::record(&mut state, &spec, &f.owner, &env, util::now()).is_err()
    );
    spec["outcome"] = json!("failure");
    crate::progress::terminal::record(&mut state, &spec, &f.owner, &env, util::now()).unwrap();
    assert_eq!(
        state["tasks"]["parent"]["cleanup_obligation"]["status"],
        "pending"
    );
    assert!(check_workspace(&state, &f.checkout()).is_err());
    // A completed terminal cleanup is the only point at which this fence retires.
    state["tasks"]["parent"]["environment"] = J::Null;
    assert!(check_workspace(&state, &f.checkout()).is_err());
    state["tasks"]["parent"]["cleanup_obligation"]["status"] = json!("released");
    check_workspace(&state, &f.checkout()).unwrap();
}

#[test]
fn explicit_command_waits_for_lifecycle_and_queue_contention_without_replaying() {
    let (f, service) = Fixture::with_server(0, "idle");
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let owner = f.owner.clone();
    let holder = std::thread::spawn(move || {
        let queue_lock = queue::locked(&owner.queue_root, false).unwrap();
        let permit = lifecycle::write_permit(&owner).unwrap();
        ready_tx.send(()).unwrap();
        std::thread::sleep(Duration::from_millis(80));
        drop(permit);
        std::thread::sleep(Duration::from_millis(120));
        drop(queue_lock);
    });
    ready_rx.recv().unwrap();
    let before = f.state()["tasks"]["parent"]["state_version"]
        .as_u64()
        .unwrap();
    assert_eq!(f.request()["phase"], "draining");
    holder.join().unwrap();
    assert_eq!(f.state()["tasks"]["parent"]["state_version"], before + 1);
    let error = locking::retry_until(Instant::now(), &mut || -> Result<()> {
        Err(FlowError::new(crate::ErrorKind::StateBusy, "held"))
    })
    .unwrap_err();
    assert_eq!(error.kind, crate::ErrorKind::StateBusy);
    assert!(error.message.contains("bounded wait"));
    service.finish();
}

const CHILD: &str = "task_runtime::human::tests::review::handoff_command_child";

#[test]
fn commands_started_inside_checkout_do_not_observe_themselves_as_writers() {
    let (f, service) = Fixture::with_server(2, "idle");
    f.request();
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", CHILD, "--ignored", "--nocapture"])
        .env("HERDR_HUMAN_TEST_COORDINATOR", f.root())
        .current_dir(f.checkout())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(f.state()["tasks"]["parent"][KEY]["phase"], "verifying");
    service.finish();
}

#[test]
#[ignore = "private subprocess fixture"]
fn handoff_command_child() {
    let root = PathBuf::from(std::env::var_os("HERDR_HUMAN_TEST_COORDINATOR").unwrap());
    let spec = json!({"id":"manual-one","actor":"operator"});
    let result = action(&root, "parent", "human-acquire", &spec).unwrap();
    assert_eq!(result["phase"], "human", "{result}");
    let returned = action(
        &root,
        "parent",
        "human-return",
        &json!({"id":"manual-one","actor":"operator","note":"finished"}),
    )
    .unwrap();
    assert_eq!(returned["phase"], "verifying");
}

#[test]
fn worker_without_observer_namespace_keeps_full_human_quiescence_available() {
    let (f, service) = Fixture::with_server(1, "idle");
    {
        let env = f.active();
        for role in ["developer", "reviewer", "recovery"] {
            let root = super::super::super::prepare(&env, role).unwrap();
            let mut runtime = util::read_state_json(&f.root(), "runtime.json").unwrap();
            runtime["root"] = json!(root);
            // These independent read-only role checkouts do not overlap the
            // transferred checkout; their retained controls are still counted.
            let cwd = f.temp.path().join(role);
            util::ensure_private_dir(&cwd, true).unwrap();
            runtime["cwd"] = json!(cwd);
            util::atomic_write_json(&root.join("runtime.json"), &runtime).unwrap();
            assert!(!root.join("recovery-observer").exists());
        }
    }
    f.request();
    let acquired = f.acquire();
    assert_eq!(acquired["phase"], "human", "{acquired}");
    assert!(
        acquired["proof"]["retained_processes"]
            .as_array()
            .unwrap()
            .len()
            >= 4
    );
    service.finish();
}

#[path = "human_optional_observers.rs"]
mod optional_observers;

#[path = "human_prepared_recovery.rs"]
mod prepared_recovery;
