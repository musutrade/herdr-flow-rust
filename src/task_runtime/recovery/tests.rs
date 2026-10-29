use super::*;

#[test]
fn human_return_is_one_recovery_incident_and_failed_verification_keeps_the_writer_fence() {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    super::super::human::tests::returned_scenario(|root, context, state| {
        let owner: Owner = serde_json::from_value(context["owner"].clone()).unwrap();
        let permit = crate::lifecycle::write_permit(&owner).unwrap();
        cycle(root, context, state, &permit).unwrap();
        let mut incident = read(root, "incident.json").unwrap().unwrap();
        assert_eq!(incident["phase"], "pending");
        assert_eq!(incident["fault"]["kind"], "human_writer_return");
        let coordinator = PathBuf::from(required(context, "coordinator").unwrap());
        incident["agent"] = json!({"runtime":coordinator,"session":"recovery-session"});
        incident["turn"] = json!("review-return");
        incident["phase"] = json!("submitted");
        let result = json!({"incident":incident["id"],"disposition":"needs_human","requested_action":"none",
            "facts":"returned change needs another inspection","actions":"read only",
            "current_state":"blocked","next_step":"reacquire and correct the change"});
        let listener = UnixListener::bind(coordinator.join("broker.sock")).unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap();
            let request: J = serde_json::from_str(&line).unwrap();
            assert_eq!(request["method"], "thread/read");
            assert_eq!(request["params"]["threadId"], "recovery-session");
            writeln!(stream, "{}", json!({"result":{"thread":{"id":"recovery-session","turns":[{
                "id":"review-return","status":"completed","items":[{"type":"agentMessage","text":result.to_string()}]}]}}})).unwrap();
        });
        agent::collect(root, context, &mut incident).unwrap();
        server.join().unwrap();
        assert_eq!(incident["phase"], "reported", "{incident}");
        let state = crate::queue::load_queue(&owner.queue_root).unwrap();
        assert_eq!(state["tasks"]["parent"]["human_writer"]["phase"], "blocked");
        report::handoff(root, context, &mut incident, &state).unwrap();
        assert_eq!(incident["phase"], "handled");
        assert_eq!(incident["handoff"]["phase"], "waiting_human");
        // No prompt, new recovery session, repeat verification or repair request.
        cycle(root, context, &state, &permit).unwrap();
        assert_eq!(read(root, "incident.json").unwrap().unwrap(), incident);
    });
}

#[test]
fn status_distinguishes_document_creation_from_unprotected_or_corrupt_state() {
    let temp = crate::util::fs::test_tempdir();
    let root = temp.path();
    util::protected::namespace(root).unwrap();
    for name in ["observer.json", "incident.json", "observer-error.json"] {
        let lock = crate::guard::locked(root).unwrap();
        // Pause the first write after mkdir, before publishing the marker.
        let document = root.join(name);
        util::ensure_private_dir(&document, true).unwrap();
        assert_eq!(snapshot(root).unwrap()["phase"], "updating");
        assert_eq!(std::fs::read_dir(&document).unwrap().count(), 0);
        drop(lock);
        // Without a writer, the same partial document must still fail closed.
        assert_eq!(snapshot(root).unwrap_err().kind, crate::ErrorKind::Guard);
        let lock = crate::guard::locked(root).unwrap();
        save(root, name, &json!({"phase":"recorded"})).unwrap();
        drop(lock);
        snapshot(root).unwrap();
    }
    let status = snapshot(root).unwrap();
    assert_eq!(status["incident"]["phase"], "recorded");
    assert_eq!(status["error"]["phase"], "recorded");
    let payload = root.join("incident.json/authority.json");
    std::fs::write(&payload, "invalid json").unwrap();
    assert_eq!(snapshot(root).unwrap_err().kind, crate::ErrorKind::Json);
    assert_eq!(std::fs::read_to_string(payload).unwrap(), "invalid json");
}

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
