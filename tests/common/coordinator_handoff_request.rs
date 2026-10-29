use super::super::tests as fixture;
use super::*;

fn authority(temp: &Path) -> (Owner, J) {
    let root = temp.join("queue");
    queue::init_isolated(&root, None, 30).unwrap();
    let mut state = queue::load_queue(&root).unwrap();
    let mut owner = fixture::owner();
    owner.queue_root = root;
    owner.queue_id = state["id"].as_str().unwrap().into();
    let mut prospective = owner.clone();
    prospective.instance_id.clear();
    drop(crate::lifecycle::acquire(&prospective, true).unwrap());
    let mut task = fixture::queue()["tasks"]["parent"].clone();
    task[KEY] = J::Null;
    task["state_version"] = json!(1);
    task["environment"]["queue_id"] = json!(owner.queue_id);
    task["environment"]["phase"] = json!("active");
    state["tasks"] = json!({"parent":task});
    state["guard_dir"] = fixture::queue()["guard_dir"].clone();
    state["receiver"] = fixture::queue()["receiver"].clone();
    // A queue-only fixture: no environment is created or adopted. Subsequent
    // commits exercise the real protected save/CAS and lifecycle locks.
    util::atomic_write_json(&owner.queue_root.join("queue.json"), &state).unwrap();
    (owner, state)
}

fn initial(owner: &Owner) -> J {
    let mut record = fixture::record(1);
    record["fence"]["owner"] = json!(owner);
    json!({"version":1,"owner":owner,"next_generation":2,"active_generation":1,"records":[record]})
}

#[test]
fn request_commit_serializes_under_ex_and_cancel_preserves_history() {
    let temp = util::fs::test_tempdir();
    let (owner, mut state) = authority(temp.path());
    let permit = exclusive(&owner).unwrap();
    // This test starts at an already admitted request to isolate CAS/EX.
    // First-admission journal checks run in the real environment tests.
    state["tasks"]["parent"][KEY] = initial(&owner);
    util::atomic_write_json(&owner.queue_root.join("queue.json"), &state).unwrap();
    let mut draining = initial(&owner);
    draining["records"][0]["phase"] = json!("draining");
    commit(&permit, &state, &draining).unwrap();
    let before_cancel = queue::load_queue(&owner.queue_root).unwrap();
    assert_eq!(before_cancel["tasks"]["parent"]["state_version"], 2);
    assert!(commit(&permit, &state, &draining).is_err());
    assert_eq!(queue::load_queue(&owner.queue_root).unwrap(), before_cancel);
    let cancelled = cancel(&draining, &json!({"request_id":"request-1"})).unwrap();
    commit(&permit, &before_cancel, &cancelled).unwrap();
    let settled = queue::load_queue(&owner.queue_root).unwrap();
    assert_eq!(
        settled["tasks"]["parent"][KEY]["records"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        settled["tasks"]["parent"][KEY]["records"][0]["phase"],
        "cancelled"
    );
    assert_eq!(settled["tasks"]["parent"]["state_version"], 3);
    assert!(crate::lifecycle::write_permit(&owner).is_err());
    drop(permit);
    crate::lifecycle::write_permit(&owner).unwrap();
}

#[test]
fn subordinate_guard_reuses_exact_ex_and_cannot_reacquire_sh_or_cross_owner() {
    let temp = util::fs::test_tempdir();
    let (owner, _) = authority(temp.path());
    let root = temp.path().join("runtime");
    util::protected::runtime_namespace(&root).unwrap();
    util::atomic_write_json(&root.join("lifecycle-owner.json"), &json!(owner)).unwrap();
    let permit = exclusive(&owner).unwrap();
    {
        let _guard = crate::guard::locked_ex(&root, &permit).unwrap();
        assert!(crate::lifecycle::runtime_observer_permit(&root).is_err());
        assert!(crate::guard::locked_ex(&root, &permit).is_err());
    }
    crate::guard::locked_ex(&root, &permit).unwrap();
    let mut other = owner.clone();
    other.instance_id = "another-instance".into();
    util::atomic_write_json(&root.join("lifecycle-owner.json"), &json!(other)).unwrap();
    assert!(crate::guard::locked_ex(&root, &permit).is_err());
    util::atomic_write_json(&root.join("lifecycle-owner.json"), &json!(owner)).unwrap();
    drop(permit);
    let shared = crate::lifecycle::write_permit(&owner).unwrap();
    assert!(crate::guard::locked_ex(&root, &shared).is_err());
}

#[test]
fn ex_wait_is_bounded_and_does_not_fall_back_to_shared() {
    let temp = util::fs::test_tempdir();
    let (owner, state) = authority(temp.path());
    let _shared = crate::lifecycle::write_permit(&owner).unwrap();
    let start = Instant::now();
    let error = exclusive(&owner).unwrap_err();
    assert_eq!(error.kind, crate::ErrorKind::StateBusy);
    assert!(start.elapsed() >= Duration::from_secs(2));
    assert!(start.elapsed() < Duration::from_secs(5));
    assert_eq!(queue::load_queue(&owner.queue_root).unwrap(), state);
}

#[test]
fn no_alias_request_or_unknown_creation_can_escape_original_intent() {
    let temp = util::fs::test_tempdir();
    let spec = json!({"request_id":"request-1","reason":"explicit settled replacement"});
    let digest = util::pyjson::sha256_json_compact(&spec);
    let mut before = initial(&fixture::owner());
    before["records"][0]["input_digest"] = json!(digest);
    assert_eq!(
        repeated(&before, &spec, &digest).unwrap(),
        Some(before.clone())
    );
    let changed = json!({"request_id":"request-1","reason":"different"});
    assert!(repeated(
        &before,
        &changed,
        &util::pyjson::sha256_json_compact(&changed)
    )
    .is_err());
    before["records"][0]["phase"] = json!("draining");
    before["records"][0]["to"]["runtime"] = json!(temp.path().join("reserved"));
    let cancel_spec = json!({"request_id":"request-1"});
    cancel(&before, &cancel_spec).unwrap();
    std::fs::create_dir(temp.path().join("reserved")).unwrap();
    assert!(cancel(&before, &cancel_spec).is_err());
    assert!(cancel(&before, &json!({"request_id":"another"})).is_err());
    assert_eq!(before["records"][0]["phase"], "draining");
}

#[test]
fn another_live_owner_or_terminal_transition_prevents_commit() {
    let temp = util::fs::test_tempdir();
    let (owner, state) = authority(temp.path());
    let permit = exclusive(&owner).unwrap();
    let mut changed = state.clone();
    changed["tasks"]["neighbor"] =
        json!({"task":"neighbor","epoch":1,"schema_version":2,"requirements":{}});
    queue::save(&owner.queue_root, &mut changed).unwrap();
    assert!(commit(&permit, &state, &initial(&owner)).is_err());
    changed["tasks"].as_object_mut().unwrap().remove("neighbor");
    changed["tasks"]["parent"]["final_outcome"] = json!("failure");
    queue::save(&owner.queue_root, &mut changed).unwrap();
    assert!(commit(&permit, &state, &initial(&owner)).is_err());
    assert!(queue::load_queue(&owner.queue_root).unwrap()["tasks"]["parent"][KEY].is_null());
}
