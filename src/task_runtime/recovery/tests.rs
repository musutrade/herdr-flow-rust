use super::*;

#[test]
fn incident_identity_deduplicates_observations_without_erasing_unknown_side_effects() {
    let first = observe::fault(
        Path::new("/task/rt/a"),
        "turn_stopped",
        &json!({"session":"s","turn":"one"}),
        json!({"error":"first sample"}),
    );
    let again = observe::fault(
        Path::new("/task/rt/a"),
        "turn_stopped",
        &json!({"session":"s","turn":"one"}),
        json!({"error":"later sample"}),
    );
    let next = observe::fault(
        Path::new("/task/rt/a"),
        "turn_stopped",
        &json!({"session":"s","turn":"two"}),
        json!({}),
    );
    assert_eq!(first["id"], again["id"]);
    assert_ne!(first["id"], next["id"]);
    assert_eq!(first["operation_replay_allowed"], false);
    assert_eq!(first["business_outcome"], "unchanged");
}

#[test]
fn observer_requires_process_ownership_and_preserves_unknown_turns() {
    let dir = crate::util::fs::test_tempdir();
    let owner = Owner {
        queue_root: "/queue".into(),
        queue_id: "q".into(),
        task_id: "t".into(),
        epoch: 1,
        instance_id: "foreign".into(),
    };
    save(
        dir.path(),
        "observer.json",
        &json!({"observer":dir.path(),"owner":owner}),
    )
    .unwrap();
    assert!(watch(dir.path()).is_err());
    assert!(!dir.path().join("watch.json").exists());
    assert!(observe::find_turn(&json!([]), &json!("missing"))
        .unwrap()
        .is_none());
    assert!(observe::find_turn(&J::Null, &json!("missing")).is_err());
    assert_eq!(
        observe::find_turn(
            &json!([{"id":"same","status":"inProgress"}]),
            &json!("same")
        )
        .unwrap()
        .unwrap()["status"],
        "inProgress"
    );
}

#[test]
fn interrupted_send_stays_unavailable_without_another_model_request() {
    let root = crate::util::fs::test_tempdir();
    let mut incident = json!({"id":"one","phase":"sending","handoff":{"phase":"pending"}});
    advance(root.path(), &json!({}), &mut incident, &json!({})).unwrap();
    assert_eq!(incident["phase"], "unavailable");
    let before = incident.clone();
    advance(root.path(), &json!({}), &mut incident, &json!({})).unwrap();
    assert_eq!(incident, before);
    assert!(incident["result"].is_null());
}

#[test]
fn repeated_fault_is_deduplicated_until_a_healthy_observation_starts_a_new_episode() {
    let root = crate::util::fs::test_tempdir();
    let fault = observe::fault(
        Path::new("/task"),
        "watcher_unavailable",
        &json!({"pid":1}),
        json!({}),
    );
    record_fault(root.path(), &json!({}), fault.clone()).unwrap();
    let first = read(root.path(), "incident.json").unwrap().unwrap();
    assert_eq!(first["seen"]["/task"], first["fault"]["id"]);
    record_fault(root.path(), &json!({}), fault.clone()).unwrap();
    assert_eq!(read(root.path(), "incident.json").unwrap().unwrap(), first);
    clear_seen(root.path()).unwrap();
    record_fault(root.path(), &json!({}), fault).unwrap();
    let later = read(root.path(), "incident.json").unwrap().unwrap();
    assert_ne!(later["id"], first["id"]);
    assert_eq!(later["fault"], first["fault"]);
}
