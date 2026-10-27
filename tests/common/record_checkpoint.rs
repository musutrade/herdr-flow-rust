use super::*;

fn write(root: &Path, name: &str, value: &J) {
    util::atomic_write_json(&root.join(name), value).unwrap();
}

fn request(id: usize, turn: &str, action: &str) -> J {
    json!({"id":id,"method":"item/tool/call","params":{"threadId":"s","turnId":turn,
        "tool":"herdr_queue","arguments":{"action":action,"delivery":"","spec":"{}"}}})
}

fn seed(root: &Path) {
    write(root, "runtime.json", &json!({"instance":"same-task"}));
    write(
        root,
        "receiver-start.json",
        &json!({"turn":"first","phase":"submitted"}),
    );
    advance(root, &json!("s"), &json!("next")).unwrap();
    write(
        root,
        "receiver-turn.json",
        &json!({"thread":"s","turn":"next"}),
    );
    write(
        root,
        "broker-events.json",
        &json!({"turns":{
        "old":{"thread":"s","turn":{"id":"old","status":"completed"}},
        "next":{"thread":"s","turn":{"id":"next","status":"inProgress"}}
    },"latest":"next","pending":{}}),
    );
}

fn pair(root: &Path, id: usize, turn: &str, action: &str) {
    let key = id.to_string();
    let request = request(id, turn, action);
    let result =
        json!({"success":true,"contentItems":[{"type":"inputText","text":"x".repeat(20_000)}]});
    let mut broker = read(root, "broker-tools.json").unwrap();
    let mut control = read(root, "control-tools.json").unwrap();
    broker[&key] = json!({"request":request,"phase":"responded","result":result});
    control[&key] = json!({"request":request,"phase":"complete","result":result});
    write(root, "broker-tools.json", &broker);
    write(root, "control-tools.json", &control);
}

#[test]
fn same_task_crosses_lifetime_call_turn_and_byte_bounds() {
    let temp = util::fs::test_tempdir();
    let root = temp.path();
    seed(root);
    for n in 0..100 {
        let turn = format!("turn-{n}");
        advance(root, &json!("s"), &json!(turn)).unwrap();
        write(
            root,
            "receiver-turn.json",
            &json!({"thread":"s","turn":turn}),
        );
        let mut events = read(root, "broker-events.json").unwrap();
        events["turns"][&turn] = json!({"thread":"s","turn":{"id":turn,"status":"completed"}});
        events["latest"] = json!(turn);
        write(root, "broker-events.json", &events);
        compact(root).unwrap();
        for offset in 0..2 {
            pair(root, n * 2 + offset, &turn, "inspect");
        }
        compact(root).unwrap();
        for name in ["broker-tools.json", "control-tools.json"] {
            let journal = read(root, name).unwrap();
            assert_eq!(journal.as_object().unwrap().len(), 2);
            assert!(std::fs::metadata(root.join(name)).unwrap().len() < 50_000);
        }
        assert_eq!(
            read(root, "broker-events.json").unwrap()["turns"]
                .as_object()
                .unwrap()
                .len(),
            2
        );
    }
    assert_eq!(read(root, "runtime.json").unwrap()["instance"], "same-task");
    assert_eq!(read(root, "receiver-start.json").unwrap()["turn"], "first");
    assert!(read(root, FILE).unwrap()["generation"].as_u64().unwrap() >= 99);
    assert!(std::fs::metadata(root.join(FILE)).unwrap().len() < 1_000);
    assert!(current_request(root, &request(0, "turn-0", "inspect")).is_err());
    assert!(current_request(root, &request(200, "turn-99", "inspect")).is_ok());
}

#[test]
fn checkpoint_keeps_uncertain_operations_and_unconsumed_obligations() {
    let temp = util::fs::test_tempdir();
    let root = temp.path();
    seed(root);
    for (id, turn, action) in [
        (1, "old", "inspect"),
        (2, "old", "run"),
        (3, "old", "finalize"),
        (4, "next", "inspect"),
        (5, "old", "deliver"),
        (6, "old", "deliver"),
        (7, "old", "inspect"),
        (8, "old", "run"),
    ] {
        pair(root, id, turn, action);
    }
    let mut broker = read(root, "broker-tools.json").unwrap();
    broker["5"]["phase"] = json!("sending");
    broker["7"]["result"] = json!({"success":false});
    write(root, "broker-tools.json", &broker);
    let mut control = read(root, "control-tools.json").unwrap();
    control["6"]["phase"] = json!("executing");
    control["8"]["collection"] = json!({"phase":"signalled"});
    write(root, "control-tools.json", &control);
    compact(root).unwrap();
    for name in ["broker-tools.json", "control-tools.json"] {
        let after = read(root, name).unwrap();
        let mut expected = if name == "broker-tools.json" {
            broker.clone()
        } else {
            control.clone()
        };
        expected.as_object_mut().unwrap().remove("1");
        assert_eq!(after, expected);
    }
    assert!(!read(root, "broker-events.json").unwrap()["turns"]["old"].is_null());
}

#[test]
fn prepared_checkpoint_resumes_every_write_prefix_without_replaying() {
    for prefix in 0..=3 {
        let temp = util::fs::test_tempdir();
        let root = temp.path();
        seed(root);
        pair(root, 1, "old", "deliver");
        let plan = prepare(root, &json!({})).unwrap();
        write(root, FILE, &plan);
        for name in FILES.into_iter().take(prefix) {
            let mut journal = read(root, name).unwrap();
            let entries = if name == "broker-events.json" {
                &mut journal["turns"]
            } else {
                &mut journal
            };
            remove(entries, &plan["files"][name]).unwrap();
            write(root, name, &journal);
        }
        assert!(ready(root).is_err());
        assert!(resume(root).unwrap());
        let complete = read(root, FILE).unwrap();
        assert_eq!(complete["phase"], "complete");
        assert!(complete["files"].is_null());
        assert!(ready(root).is_ok());
        assert!(resume(root).unwrap());
        assert_eq!(read(root, FILE).unwrap(), complete);
        assert_eq!(read(root, "control-tools.json").unwrap(), json!({}));
        assert_eq!(read(root, "broker-tools.json").unwrap(), json!({}));
        assert!(current_request(root, &request(1, "old", "deliver")).is_err());
    }
}

#[test]
fn changed_evidence_and_service_lock_prevent_partial_reclamation() {
    let temp = util::fs::test_tempdir();
    let root = temp.path();
    seed(root);
    pair(root, 1, "old", "inspect");
    let lock = util::FileLock::acquire(&root.join("control-service.lock"), false)
        .unwrap()
        .unwrap();
    compact(root).unwrap();
    assert!(!root.join(FILE).exists());
    let plan = prepare(root, &json!({})).unwrap();
    write(root, FILE, &plan);
    assert!(!resume(root).unwrap());
    drop(lock);
    let control = read(root, "control-tools.json").unwrap();
    let mut broker = read(root, "broker-tools.json").unwrap();
    broker["1"]["phase"] = json!("sending");
    write(root, "broker-tools.json", &broker);
    assert!(resume(root).is_err());
    assert_eq!(read(root, "control-tools.json").unwrap(), control);
    assert_eq!(read(root, "broker-tools.json").unwrap(), broker);
    assert_eq!(read(root, FILE).unwrap(), plan);
    std::fs::remove_file(root.join("broker-tools.json")).unwrap();
    assert!(resume(root).is_err());
    assert_eq!(read(root, "control-tools.json").unwrap(), control);
    assert!(!root.join("broker-tools.json").exists());
}

#[test]
fn events_cross_bound_in_one_drain_but_pending_approval_needs_exact_resolution() {
    let temp = util::fs::test_tempdir();
    let root = temp.path();
    seed(root);
    let mut journal = observation_journal(root).unwrap();
    let mut events = vec![
        json!({"id":99,"method":"item/commandExecution/requestApproval",
        "params":{"threadId":"s","turnId":"old"}}),
    ];
    for i in 0..130 {
        events.push(json!({"method":"turn/completed","params":{"threadId":"s", "turn":{"id":format!("t-{i}"),"status":"completed"}}}));
    }
    save_events(root, &mut journal, events).unwrap();
    let mut journal = observation_journal(root).unwrap();
    assert!(!journal["turns"]["old"].is_null());
    assert!(journal["turns"]["t-0"].is_null());
    assert!(!journal["turns"]["t-129"].is_null());
    let foreign =
        json!({"method":"serverRequest/resolved","params":{"threadId":"other","requestId":99}});
    save_events(root, &mut journal, vec![foreign]).unwrap();
    assert!(!observation_journal(root).unwrap()["pending"]["99"].is_null());
    let resolved =
        json!({"method":"serverRequest/resolved","params":{"threadId":"s","requestId":99}});
    save_events(root, &mut journal, vec![resolved]).unwrap();
    let after = observation_journal(root).unwrap();
    assert!(after["pending"]["99"].is_null());
    assert!(after["turns"]["old"].is_null());
}

#[test]
fn capacity_preserves_existing_records_and_reports_a_recoverable_blocker() {
    let temp = util::fs::test_tempdir();
    let root = temp.path();
    seed(root);
    for id in 0..64 {
        super::super::tools::record(root, &request(id, "next", "inspect")).unwrap();
    }
    let before = read(root, "broker-tools.json").unwrap();
    assert!(super::super::tools::record(root, &request(64, "next", "inspect")).is_err());
    assert_eq!(read(root, "broker-tools.json").unwrap(), before);
    let capacity = read(root, "record-capacity.json").unwrap();
    assert_eq!(capacity["phase"], "blocked");
    assert_eq!(capacity["entries"], 65);
    let mut oversized = json!({"one":{"data":"x".repeat(1024 * 1024)}});
    assert!(super::super::capacity::tools(root, "control-tools.json", &oversized).is_err());
    oversized["one"]["data"] = json!("small");
    super::super::capacity::tools(root, "control-tools.json", &oversized).unwrap();
    assert_eq!(read(root, "runtime.json").unwrap()["instance"], "same-task");
}

#[test]
fn worker_collection_requires_application_of_the_exact_binding() {
    let temp = util::fs::test_tempdir();
    let root = temp.path();
    seed(root);
    pair(root, 1, "old", "run");
    let queue_root = root.join("queue");
    let mut q = crate::queue::init(&queue_root, None, 30).unwrap();
    write(
        root,
        "runtime.json",
        &json!({"instance":"same-task","lifecycle_owner":{"queue_root":queue_root,"queue_id":q["id"]}}),
    );
    let binding = json!({"task":"dev","epoch":2,"assignment":"a2"});
    for applied in [false, true] {
        let mut control = read(root, "control-tools.json").unwrap();
        let mut broker = read(root, "broker-tools.json").unwrap();
        control["1"]["request"]["params"]["arguments"]["spec"] = json!("{\"wait\":\"dev\"}");
        broker["1"]["request"] = control["1"]["request"].clone();
        control["1"]["collection"] = json!({"phase":"signalled","binding":binding});
        write(root, "control-tools.json", &control);
        write(root, "broker-tools.json", &broker);
        q["events"] = json!({"previous":{"wait":"dev","applied":true,"binding":{"task":"dev","epoch":1,"assignment":"a1"}},
            "current":{"wait":"dev","applied":applied,"binding":binding}});
        crate::queue::save(&queue_root, &mut q).unwrap();
        compact(root).unwrap();
        assert_eq!(
            read(root, "control-tools.json").unwrap()["1"].is_null(),
            applied
        );
        assert_eq!(
            read(root, "broker-tools.json").unwrap()["1"].is_null(),
            applied
        );
    }
}

#[test]
fn failed_external_operations_and_nonterminal_turns_keep_their_evidence() {
    let temp = util::fs::test_tempdir();
    let root = temp.path();
    seed(root);
    for (id, action) in [(1, "run"), (2, "deliver"), (3, "finalize"), (4, "inspect")] {
        pair(root, id, "old", action);
    }
    for name in ["control-tools.json", "broker-tools.json"] {
        let mut journal = read(root, name).unwrap();
        for id in ["1", "2", "3"] {
            journal[id]["result"]["success"] = json!(false);
        }
        write(root, name, &journal);
    }
    let mut events = read(root, "broker-events.json").unwrap();
    events["turns"]["old"]["turn"]["status"] = json!("failed");
    write(root, "broker-events.json", &events);
    compact(root).unwrap();
    assert_eq!(
        read(root, "control-tools.json")
            .unwrap()
            .as_object()
            .unwrap()
            .len(),
        4
    );
    events["turns"]["old"]["turn"]["status"] = json!("completed");
    write(root, "broker-events.json", &events);
    compact(root).unwrap();
    assert_eq!(
        read(root, "control-tools.json")
            .unwrap()
            .as_object()
            .unwrap()
            .len(),
        3
    );
}

#[test]
fn native_turn_fence_follows_only_the_registered_receiver_event() {
    let temp = util::fs::test_tempdir();
    let root = temp.path();
    seed(root);
    let before = read(root, "broker-turn.json").unwrap();
    for params in [
        json!({"threadId":"other","turn":{"id":"native"}}),
        json!({"threadId":"s","turn":{"id":"unregistered"}}),
    ] {
        native_turn(root, &json!({"method":"turn/started","params":params})).unwrap();
        assert_eq!(read(root, "broker-turn.json").unwrap(), before);
    }
    write(
        root,
        "receiver-turn.json",
        &json!({"thread":"s","turn":"native"}),
    );
    native_turn(
        root,
        &json!({"method":"turn/started","params":{"threadId":"s","turn":{"id":"native"}}}),
    )
    .unwrap();
    assert!(current_request(root, &request(2, "next", "run")).is_err());
    assert!(current_request(root, &request(3, "native", "inspect")).is_ok());
    assert!(advance(root, &json!("s"), &J::Null).is_err());
    assert_eq!(read(root, "broker-turn.json").unwrap()["turn"], "native");
}
