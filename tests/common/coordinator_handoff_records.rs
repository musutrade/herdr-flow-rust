use super::*;

#[path = "coordinator_handoff_review_regressions.rs"]
mod review_regressions;

fn owner() -> Owner {
    Owner {
        queue_root: "/queue".into(),
        queue_id: "queue".into(),
        task_id: "parent".into(),
        epoch: 1,
        instance_id: "instance".into(),
    }
}

fn model() -> J {
    json!({"providerID":"openai","id":"fixture","reasoning_effort":"high"})
}

fn target(n: usize, populated: bool) -> J {
    let root = format!("/scratch/rt/coordinator-{n}");
    let session = format!("00000000-0000-0000-0000-{n:012}");
    let mut target =
        json!({"runtime":root,"guard":format!("{root}/guard"),"session":null,"scope":null});
    if populated {
        target["session"] = json!(session);
        target["scope"] = json!({"task":"parent","role":"coordinator","pane":null,"terminal":null,"socket":null,"cwd":"/workspace/checkout",
            "codex_runtime":{"adapter":"codex","session":session,"runtime":root,"instance":"instance","cwd":"/workspace/checkout","model":model()}});
    }
    target
}

fn notice(generation: usize, kind: &str) -> J {
    json!({"wait":format!("handoff-{generation}-{kind}"),"event":null,"delivery":null,"turn":null,"phase":"pending"})
}

fn record(n: usize) -> J {
    let from = target(n - 1, true);
    let to = target(n, false);
    let policy = json!({"type":"workspaceWrite","writableRoots":["/workspace/checkout"],"networkAccess":false});
    json!({"generation":n,"request_id":format!("request-{n}"),"input_digest":"a".repeat(64),"reason":"explicit settled replacement","phase":"requested",
        "from":from,"to":to,"configuration":{"provider":"openai","model":model(),"effort":"high","approval":"on-request","sandbox":policy,
            "cwd":"/workspace/checkout","upstream_endpoint":{"path":"/upstream.sock","dev":1,"ino":2,"uid":1000},"role_models":{"developer":model(),"reviewer":model()},"driver":"/driver",
            "business_session_configuration":{"model":"fixture","modelProvider":"openai","cwd":"/workspace/checkout","approvalPolicy":"on-request","sandbox":policy,"reasoningEffort":"high"}},
        "snapshot":null,"fence":{"generation":n,"owner":owner(),"old_runtime":from["runtime"],"new_runtime":to["runtime"]},"binding":null,
        "notices":{"accept":notice(n,"accept"),"ready":notice(n,"ready")},"recovery":null,"retirement":null})
}

fn queue() -> J {
    json!({"guard_dir":target(0,true)["guard"],"receiver":target(0,true)["scope"],"id":"queue","lifecycle":{"queue_root":"/queue"},"tasks":{"parent":{"task":"parent","epoch":1,"schema_version":2,"requirements":{},
        "environment":{"phase":"active","queue_id":"queue","task_id":"parent","epoch":1,"instance_id":"instance","checkout":"/workspace/checkout","creation_seed":{"temp_root":"/scratch"}},
        KEY:{"version":1,"owner":owner(),"next_generation":2,"active_generation":1,"records":[record(1)]}}}})
}

fn align_receiver(q: &mut J) {
    let record = q["tasks"]["parent"][KEY]["records"]
        .as_array()
        .unwrap()
        .last()
        .unwrap();
    let target = if record["binding"].is_null() {
        &record["from"]
    } else {
        &record["to"]
    };
    let guard = target["guard"].clone();
    let scope = target["scope"].clone();
    q["guard_dir"] = guard;
    q["receiver"] = scope;
}

fn entry(queue: &mut J) -> &mut J {
    &mut queue["tasks"]["parent"][KEY]["records"][0]
}

fn complete(mut record: J) -> J {
    let n = record["generation"].as_u64().unwrap() as usize;
    record["to"] = target(n, true);
    record["phase"] = json!("complete");
    record["snapshot"] = json!({"path":format!("/scratch/rt/coordinator-{}/handoff-{n}.json",n-1),"sha256":"b".repeat(64),"bytes":123});
    record["binding"] = json!({"transaction_id":format!("binding-{n}"),"from_scope":record["from"]["scope"],"to_scope":record["to"]["scope"],"committed_at":1.0});
    record["retirement"] = json!({"path":format!("/scratch/rt/coordinator-{}/retirement-{n}.json",n-1),"sha256":"c".repeat(64),"phase":"complete"});
    for name in ["accept", "ready"] {
        record["notices"][name]["phase"] = json!("acknowledged");
        record["notices"][name]["event"] = json!(format!("event-{n}-{name}"));
        record["notices"][name]["delivery"] = json!(format!("delivery-{n}-{name}"));
        record["notices"][name]["turn"] = json!(format!("turn-{n}-{name}"));
    }
    record
}

#[test]
fn absent_extension_does_not_require_parent_or_active_runtime() {
    for task in [
        json!({}),
        json!({"schema_version":2,"requirements":{}}),
        json!({"lineage":{"requirement":"parent"}}),
        json!({"final_outcome":"success"}),
    ] {
        for null in [false, true] {
            let mut q = json!({"tasks":{"ordinary":task}});
            if null {
                q["tasks"]["ordinary"][KEY] = J::Null;
            }
            validate_queue(&q).unwrap();
        }
    }
    validate_queue(&json!({})).unwrap();
}

#[test]
fn owner_parent_and_unknown_fields_fail_closed() {
    validate_queue(&queue()).unwrap();
    for (path, value) in [
        ("/tasks/parent/schema_version", json!(1)),
        ("/tasks/parent/requirements", J::Null),
        ("/tasks/parent/receiver_handoff/owner/epoch", json!(2)),
        ("/tasks/parent/receiver_handoff/version", json!(2)),
        ("/tasks/parent/receiver_handoff/active_generation", J::Null),
        ("/tasks/parent/receiver_handoff/next_generation", json!(9)),
    ] {
        let mut q = queue();
        *q.pointer_mut(path).unwrap() = value;
        assert!(validate_queue(&q).is_err(), "{path}");
    }
    let mut q = queue();
    entry(&mut q)["unexpected"] = json!(true);
    assert!(validate_queue(&q).is_err());
}

#[test]
fn successor_paths_sessions_and_configuration_cannot_escape_owner() {
    for (path, value) in [
        ("/to/runtime", json!("/foreign/rt/next")),
        ("/to/guard", json!("/scratch/rt/coordinator-0/guard")),
        ("/to/runtime", json!("/scratch/rt/../coordinator-1")),
        ("/configuration/approval", json!("never")),
        ("/fence/owner/instance_id", json!("foreign")),
        ("/input_digest", json!("unknown")),
    ] {
        let mut q = queue();
        *entry(&mut q).pointer_mut(path).unwrap() = value;
        assert!(validate_queue(&q).is_err(), "{path}");
    }
    let mut q = queue();
    entry(&mut q)["to"] = target(0, true);
    assert!(validate_queue(&q).is_err());
    let mut q = queue();
    *entry(&mut q) = complete(record(1));
    q["tasks"]["parent"][KEY]["active_generation"] = J::Null;
    align_receiver(&mut q);
    let next = entry(&mut q);
    // A textual alias must not evade ancestor de-duplication while selecting
    // exactly the predecessor's physical runtime directory.
    next["to"]["runtime"] = json!("/scratch//rt/coordinator-0");
    next["to"]["guard"] = json!("/scratch//rt/coordinator-0/guard");
    next["to"]["scope"]["codex_runtime"]["runtime"] = next["to"]["runtime"].clone();
    next["fence"]["new_runtime"] = next["to"]["runtime"].clone();
    next["binding"]["to_scope"] = next["to"]["scope"].clone();
    assert!(validate_queue(&q).is_err());
}

#[test]
fn eight_completed_generations_are_bounded_and_cycles_are_rejected() {
    let mut q = queue();
    q["tasks"]["parent"][KEY]["records"] =
        json!((1..=8).map(|n| complete(record(n))).collect::<Vec<_>>());
    q["tasks"]["parent"][KEY]["next_generation"] = json!(9);
    q["tasks"]["parent"][KEY]["active_generation"] = J::Null;
    align_receiver(&mut q);
    validate_queue(&q).unwrap();
    let good = q.clone();
    q["tasks"]["parent"][KEY]["records"]
        .as_array_mut()
        .unwrap()
        .push(complete(record(9)));
    q["tasks"]["parent"][KEY]["next_generation"] = json!(10);
    assert!(validate_queue(&q).is_err());
    let mut q = good;
    let last = &mut q["tasks"]["parent"][KEY]["records"][7];
    last["to"] = target(0, true);
    last["fence"]["new_runtime"] = last["to"]["runtime"].clone();
    last["binding"]["to_scope"] = last["to"]["scope"].clone();
    assert!(validate_queue(&q).is_err());
}

#[test]
fn saved_fence_cannot_be_erased_rewound_or_rebound() {
    let before = queue();
    for action in ["null", "erase", "skip", "change", "reservation"] {
        let mut next = before.clone();
        match action {
            "null" => next["tasks"]["parent"][KEY] = J::Null,
            "erase" => {
                next["tasks"]["parent"].as_object_mut().unwrap().remove(KEY);
            }
            "skip" => entry(&mut next)["phase"] = json!("creating"),
            "change" => entry(&mut next)["reason"] = json!("another reason"),
            _ => entry(&mut next)["to"]["runtime"] = json!("/scratch/rt/another"),
        }
        assert!(validate_save(&before, &next).is_err(), "{action}");
    }
    let mut draining = before.clone();
    entry(&mut draining)["phase"] = json!("draining");
    validate_queue(&draining).unwrap();
    validate_save(&before, &draining).unwrap();
    let mut cancelled = draining.clone();
    entry(&mut cancelled)["phase"] = json!("cancelled");
    cancelled["tasks"]["parent"][KEY]["active_generation"] = J::Null;
    validate_queue(&cancelled).unwrap();
    validate_save(&draining, &cancelled).unwrap();
    assert!(validate_save(&cancelled, &before).is_err());
}

#[test]
fn terminal_retirement_is_distinct_from_erasing_an_active_fence() {
    let mut before = queue();
    let mut after = before.clone();
    after["tasks"] = json!({});
    assert!(validate_save(&before, &after).is_err());
    before["tasks"]["parent"]["final_outcome"] = json!("failure");
    before["tasks"]["parent"]["cleanup_obligation"] = json!({"owner":owner(),"status":"released"});
    validate_queue(&before).unwrap();
    validate_save(&before, &after).unwrap();
}

#[test]
fn another_generation_requires_original_ready_ack_and_preserves_frozen_prefix() {
    let mut before = queue();
    *entry(&mut before) = complete(record(1));
    before["tasks"]["parent"][KEY]["active_generation"] = J::Null;
    align_receiver(&mut before);
    let mut after = before.clone();
    after["tasks"]["parent"][KEY]["records"]
        .as_array_mut()
        .unwrap()
        .push(record(2));
    after["tasks"]["parent"][KEY]["next_generation"] = json!(3);
    after["tasks"]["parent"][KEY]["active_generation"] = json!(2);
    validate_queue(&after).unwrap();
    validate_save(&before, &after).unwrap();
    entry(&mut before)["notices"]["ready"]["phase"] = json!("submitted");
    assert!(validate_save(&before, &after).is_err());
    for (path, value) in [
        ("/snapshot/sha256", json!("e".repeat(64))),
        ("/binding/transaction_id", json!("replacement")),
        ("/to/session", json!("00000000-0000-0000-0000-000000000099")),
        ("/notices/accept/delivery", json!("another-delivery")),
        ("/notices/accept/phase", json!("sending")),
        ("/retirement/sha256", json!("d".repeat(64))),
    ] {
        let mut changed = before.clone();
        *entry(&mut changed).pointer_mut(path).unwrap() = value;
        assert!(validate_save(&before, &changed).is_err(), "{path}");
    }
}

#[test]
fn recovery_binding_remains_the_original_role_and_target() {
    let mut before = queue();
    let entry = entry(&mut before);
    entry["configuration"]["role_models"]["recovery"] = model();
    entry["recovery"] = json!({"observer":"/scratch/rt/coordinator-0/recovery-observer",
        "origin_coordinator":"/scratch/rt/coordinator-0","phase":"pending",
        "from_target":{"generation":0,"runtime":entry["from"]["runtime"],"scope":entry["from"]["scope"],"transaction_id":null},
        "to_target":{"generation":1,"runtime":entry["to"]["runtime"],"scope":null,"transaction_id":null}});
    validate_queue(&before).unwrap();
    let mut next = before.clone();
    super::tests::entry(&mut next)["phase"] = json!("draining");
    validate_save(&before, &next).unwrap();
    for (path, value) in [
        ("/observer", json!("/scratch/rt/other/recovery-observer")),
        ("/from_target/runtime", json!("/scratch/rt/other")),
        ("/phase", json!("unknown")),
    ] {
        let mut invalid = before.clone();
        *super::tests::entry(&mut invalid)["recovery"]
            .pointer_mut(path)
            .unwrap() = value;
        assert!(validate_queue(&invalid).is_err(), "{path}");
    }
    super::tests::entry(&mut next)["recovery"]["origin_coordinator"] = json!("/another");
    assert!(validate_save(&before, &next).is_err());
}

#[test]
fn phase_requires_recorded_creation_ack_and_retirement_evidence() {
    let mut q = queue();
    *entry(&mut q) = complete(record(1));
    q["tasks"]["parent"][KEY]["active_generation"] = J::Null;
    align_receiver(&mut q);
    for (path, value) in [
        ("/snapshot", J::Null),
        ("/snapshot/bytes", json!(16385)),
        ("/snapshot/path", json!("/outside/evidence")),
        ("/binding", J::Null),
        ("/to/session", J::Null),
        ("/notices/accept/event", J::Null),
        ("/notices/accept/delivery", J::Null),
        ("/notices/accept/phase", json!("submitted")),
        ("/retirement", J::Null),
        ("/retirement/phase", json!("retiring")),
    ] {
        let mut changed = q.clone();
        *entry(&mut changed).pointer_mut(path).unwrap() = value;
        assert!(validate_queue(&changed).is_err(), "{path}");
    }
    let mut changed = queue();
    entry(&mut changed)["reason"] = json!("x".repeat(1025));
    assert!(validate_queue(&changed).is_err());
}

#[test]
fn queue_load_and_save_enforce_the_extension_before_writing() {
    let temp = util::fs::test_tempdir();
    let root = temp.path().join("queue");
    crate::queue::init_isolated(&root, None, 30).unwrap();
    let mut original = crate::queue::load_queue(&root).unwrap();
    let mut fixture = queue();
    let mut binding = owner();
    binding.queue_root = root.clone();
    binding.queue_id = original["id"].as_str().unwrap().into();
    fixture["tasks"]["parent"][KEY]["owner"] = json!(binding);
    fixture["tasks"]["parent"]["environment"]["queue_id"] = json!(binding.queue_id);
    entry(&mut fixture)["fence"]["owner"] = json!(binding);
    original["tasks"] = fixture["tasks"].clone();
    original["guard_dir"] = fixture["guard_dir"].clone();
    original["receiver"] = fixture["receiver"].clone();
    // Seed a protected authority snapshot; no runtime or environment is created.
    util::atomic_write_json(&root.join("queue.json"), &original).unwrap();
    crate::queue::load_queue(&root).unwrap();
    let mut erased = original.clone();
    erased["tasks"]["parent"][KEY] = J::Null;
    assert!(crate::queue::save(&root, &mut erased).is_err());
    assert_eq!(crate::queue::load_queue(&root).unwrap(), original);
    let mut corrupt = original;
    entry(&mut corrupt)["unexpected"] = json!(true);
    util::atomic_write_json(&root.join("queue.json"), &corrupt).unwrap();
    assert!(crate::queue::load_queue(&root).is_err());
}

#[test]
fn intention_or_complete_label_cannot_grant_new_work_before_restore_driver() {
    let q = queue();
    check_workspace(&q, Path::new("/independent/checkout")).unwrap();
    assert!(check_workspace(&q, Path::new("/workspace/checkout/file")).is_err());
    for action in ["inspect", "ack", "apply"] {
        admit_control(&q, &owner(), action).unwrap();
    }
    for action in [
        "run",
        "lineage",
        "deliver",
        "publish",
        "stage-complete",
        "finalize",
    ] {
        assert!(admit_control(&q, &owner(), action).is_err(), "{action}");
    }
    let mut finished = q.clone();
    *entry(&mut finished) = complete(record(1));
    finished["tasks"]["parent"][KEY]["active_generation"] = J::Null;
    align_receiver(&mut finished);
    assert!(check_workspace(&finished, Path::new("/workspace/checkout")).is_err());
    assert!(admit_control(&finished, &owner(), "apply").is_err());
    let mut cancelled = q;
    entry(&mut cancelled)["phase"] = json!("cancelled");
    cancelled["tasks"]["parent"][KEY]["active_generation"] = J::Null;
    check_workspace(&cancelled, Path::new("/workspace/checkout")).unwrap();
    admit_control(&cancelled, &owner(), "run").unwrap();
}

#[test]
fn native_admission_keeps_reads_and_original_replies_but_refuses_new_turns() {
    let temp = util::fs::test_tempdir();
    let root = temp.path().join("queue");
    crate::queue::init_isolated(&root, None, 30).unwrap();
    let mut q = crate::queue::load_queue(&root).unwrap();
    let mut fixture = queue();
    let mut owner = owner();
    owner.queue_root = root.clone();
    owner.queue_id = q["id"].as_str().unwrap().into();
    fixture["tasks"]["parent"][KEY]["owner"] = json!(owner);
    fixture["tasks"]["parent"]["environment"]["queue_id"] = json!(owner.queue_id);
    entry(&mut fixture)["fence"]["owner"] = json!(owner);
    q["tasks"] = fixture["tasks"].clone();
    q["guard_dir"] = fixture["guard_dir"].clone();
    q["receiver"] = fixture["receiver"].clone();
    util::atomic_write_json(&root.join("queue.json"), &q).unwrap();
    let runtime = temp.path().join("runtime");
    util::ensure_private_dir(&runtime, true).unwrap();
    util::atomic_write_json(
        &runtime.join("runtime.json"),
        &json!({"root":runtime,
        "lifecycle_owner":owner,"cwd":"/workspace/checkout","instance":"instance"}),
    )
    .unwrap();
    for method in ["thread/read", "turn/interrupt", "tool/respond"] {
        crate::task_runtime::human::admit_native(&runtime, &json!({"method":method})).unwrap();
    }
    let turn = json!({"method":"turn/start","params":{"threadId":"original"}});
    assert!(crate::task_runtime::human::admit_native(&runtime, &turn).is_err());
    assert!(!crate::task_runtime::human::automatic_input_allowed(&runtime).unwrap());
    entry(&mut q)["phase"] = json!("cancelled");
    q["tasks"]["parent"][KEY]["active_generation"] = J::Null;
    align_receiver(&mut q);
    util::atomic_write_json(&root.join("queue.json"), &q).unwrap();
    crate::task_runtime::human::admit_native(&runtime, &turn).unwrap();
    assert!(crate::task_runtime::human::automatic_input_allowed(&runtime).unwrap());
    entry(&mut q)["phase"] = json!("corrupt");
    util::atomic_write_json(&root.join("queue.json"), &q).unwrap();
    assert!(crate::task_runtime::human::admit_native(&runtime, &turn).is_err());
    crate::task_runtime::human::admit_native(&runtime, &json!({"method":"thread/read"})).unwrap();
}
