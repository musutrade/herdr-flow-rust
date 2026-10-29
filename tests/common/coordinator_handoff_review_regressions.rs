use super::*;

#[test]
fn queue_anchor_and_complete_predecessor_identity_cannot_be_self_declared() {
    let mut q = queue();
    entry(&mut q)["from"] = target(99, true);
    entry(&mut q)["fence"]["old_runtime"] = target(99, true)["runtime"].clone();
    assert!(validate_queue(&q)
        .unwrap_err()
        .message
        .contains("queue receiver"));
    let mut q = queue();
    *entry(&mut q) = complete(record(1));
    q["tasks"]["parent"][KEY]["records"]
        .as_array_mut()
        .unwrap()
        .push(record(2));
    q["tasks"]["parent"][KEY]["next_generation"] = json!(3);
    q["tasks"]["parent"][KEY]["active_generation"] = json!(2);
    align_receiver(&mut q);
    validate_queue(&q).unwrap();
    q["tasks"]["parent"][KEY]["records"][1]["from"]["guard"] =
        json!("/scratch/rt/coordinator-1/other-guard");
    align_receiver(&mut q);
    assert!(validate_queue(&q)
        .unwrap_err()
        .message
        .contains("disconnected"));
}

#[test]
fn invalid_permissions_remain_invalid_when_the_business_snapshot_agrees() {
    for approval in [J::Null, json!(true), json!("never"), json!("unknown")] {
        let mut q = queue();
        let config = &mut entry(&mut q)["configuration"];
        config["approval"] = approval.clone();
        config["business_session_configuration"]["approvalPolicy"] = approval;
        assert!(validate_queue(&q).is_err());
    }
    for sandbox in [
        J::Null,
        json!({}),
        json!({"type":"dangerFullAccess"}),
        json!({"type":"workspaceWrite","networkAccess":false,"writableRoots":["/foreign"]}),
        json!({"type":"workspaceWrite","networkAccess":"false","writableRoots":[]}),
        json!({"type":"workspaceWrite","networkAccess":false,"writableRoots":[],"excludeSlashTmp":1}),
        json!({"type":"workspaceWrite","networkAccess":false,"writableRoots":[],"unknown":true}),
    ] {
        let mut q = queue();
        let config = &mut entry(&mut q)["configuration"];
        config["sandbox"] = sandbox.clone();
        config["business_session_configuration"]["sandbox"] = sandbox;
        assert!(validate_queue(&q).is_err());
    }
    let mut q = queue();
    for key in ["sandbox", "business_session_configuration"] {
        if key == "sandbox" {
            entry(&mut q)["configuration"][key]["writableRoots"] = json!([]);
        } else {
            entry(&mut q)["configuration"][key]["sandbox"]["writableRoots"] = json!([]);
        }
    }
    validate_queue(&q).unwrap();
}

#[test]
fn unspecified_effort_must_also_match_the_frozen_business_snapshot() {
    let mut q = queue();
    let record = entry(&mut q);
    record["configuration"]["model"]
        .as_object_mut()
        .unwrap()
        .remove("reasoning_effort");
    record["configuration"]["effort"] = J::Null;
    record["from"]["scope"]["codex_runtime"]["model"] = record["configuration"]["model"].clone();
    align_receiver(&mut q);
    // An unspecified model must not silently replace the original explicit
    // live effort, even if the self-declared model/effort agree with each other.
    assert!(validate_queue(&q)
        .unwrap_err()
        .message
        .contains("original effort changed"));
    entry(&mut q)["configuration"]["business_session_configuration"]["reasoningEffort"] = J::Null;
    validate_queue(&q).unwrap();
    entry(&mut q)["configuration"]["business_session_configuration"]
        .as_object_mut()
        .unwrap()
        .remove("reasoningEffort");
    validate_queue(&q).unwrap();
}

#[test]
fn endpoint_role_and_presentation_fields_have_real_types() {
    for (field, value) in [
        ("path", json!("relative.sock")),
        ("dev", J::Null),
        ("ino", json!(0)),
        ("uid", json!("1000")),
    ] {
        let mut q = queue();
        entry(&mut q)["configuration"]["upstream_endpoint"][field] = value;
        assert!(validate_queue(&q).is_err());
    }
    for roles in [
        J::Null,
        json!({}),
        json!({"developer":model(),"reviewer":{"providerID":"openai","id":""}}),
    ] {
        let mut q = queue();
        entry(&mut q)["configuration"]["role_models"] = roles;
        assert!(validate_queue(&q).is_err());
    }
    // All-null is the actual native task receiver schema; no presentation is
    // inferred. A supplied identity is complete, bounded, and typed instead.
    validate_queue(&queue()).unwrap();
    for (field, value) in [
        ("pane", json!(7)),
        ("terminal", json!("term")),
        ("socket", json!({})),
    ] {
        let mut q = queue();
        entry(&mut q)["from"]["scope"][field] = value;
        align_receiver(&mut q);
        assert!(validate_queue(&q).is_err());
    }
    let mut q = queue();
    for (field, value) in [
        ("pane", "w1:p1"),
        ("terminal", "term_1"),
        ("socket", "/herdr.sock"),
    ] {
        entry(&mut q)["from"]["scope"][field] = json!(value);
    }
    align_receiver(&mut q);
    validate_queue(&q).unwrap();
}

fn with_recovery(mut record: J, predecessor: &J) -> J {
    record["configuration"]["role_models"]["recovery"] = model();
    record["recovery"] = json!({"observer":"/scratch/rt/coordinator-0/recovery-observer",
        "origin_coordinator":"/scratch/rt/coordinator-0","phase":if record["phase"] == "complete" {"bound"} else {"pending"},
        "from_target":predecessor,
        "to_target":{"generation":record["generation"],"runtime":record["to"]["runtime"],
        "scope":record["to"]["scope"],"transaction_id":record["binding"]["transaction_id"]}});
    record
}

fn origin() -> J {
    json!({"generation":0,"runtime":target(0,true)["runtime"],"scope":target(0,true)["scope"],"transaction_id":null})
}

#[test]
fn recovery_uses_original_origin_and_exact_committed_generation_transaction() {
    let mut q = queue();
    *entry(&mut q) = with_recovery(record(1), &origin());
    validate_queue(&q).unwrap();
    for (field, value) in [
        ("/from_target/generation", json!(1)),
        ("/from_target/transaction_id", json!("foreign")),
        ("/to_target/generation", J::Null),
        ("/to_target/transaction_id", json!("foreign")),
        ("/phase", json!("bound")),
    ] {
        let mut invalid = q.clone();
        *entry(&mut invalid)["recovery"].pointer_mut(field).unwrap() = value;
        assert!(validate_queue(&invalid).is_err(), "{field}");
    }
    let mut invalid = q.clone();
    entry(&mut invalid)["recovery"]["origin_coordinator"] = json!("/another/task");
    entry(&mut invalid)["recovery"]["observer"] = json!("/another/task/recovery-observer");
    assert!(validate_queue(&invalid).is_err());
    let first = with_recovery(complete(record(1)), &origin());
    let second = with_recovery(record(2), &first["recovery"]["to_target"]);
    q["tasks"]["parent"][KEY]["records"] = json!([first, second]);
    q["tasks"]["parent"][KEY]["next_generation"] = json!(3);
    q["tasks"]["parent"][KEY]["active_generation"] = json!(2);
    align_receiver(&mut q);
    validate_queue(&q).unwrap();
    q["tasks"]["parent"][KEY]["records"][1]["recovery"]["from_target"]["transaction_id"] = J::Null;
    assert!(validate_queue(&q).is_err());
}

#[test]
fn ready_registration_and_first_retirement_plan_digest_are_not_optional() {
    let mut before = queue();
    *entry(&mut before) = complete(record(1));
    before["tasks"]["parent"][KEY]["active_generation"] = J::Null;
    align_receiver(&mut before);
    let mut missing = before.clone();
    entry(&mut missing)["notices"]["ready"] = notice(1, "ready");
    assert!(validate_queue(&missing).is_err());
    entry(&mut before)["phase"] = json!("retiring");
    entry(&mut before)["notices"]["ready"] = notice(1, "ready");
    entry(&mut before)["retirement"]["phase"] = json!("planned");
    before["tasks"]["parent"][KEY]["active_generation"] = json!(1);
    validate_queue(&before).unwrap();
    let mut next = before.clone();
    entry(&mut next)["retirement"]["phase"] = json!("sessions");
    validate_queue(&next).unwrap();
    validate_save(&before, &next).unwrap();
    entry(&mut next)["retirement"]["sha256"] = json!("d".repeat(64));
    validate_queue(&next).unwrap();
    assert!(validate_save(&before, &next).is_err());
}

#[test]
fn terminal_history_loads_but_cannot_gain_or_advance_a_new_handoff() {
    for field in ["final_outcome", "cleanup_obligation"] {
        let mut after = queue();
        after["tasks"]["parent"][field] = json!("terminal");
        let mut before = after.clone();
        before["tasks"]["parent"][KEY] = J::Null;
        validate_queue(&after).unwrap();
        assert!(validate_save(&before, &after).is_err());
        let mut advanced = after.clone();
        entry(&mut advanced)["phase"] = json!("draining");
        assert!(validate_save(&after, &advanced).is_err());
        validate_save(&after, &after).unwrap();
    }
}

fn relocated(value: &mut J, temp: &Path) {
    match value {
        J::String(text) => {
            for prefix in ["/scratch", "/workspace"] {
                if text.starts_with(prefix) {
                    *text = format!("{}{}", temp.display(), text);
                    break;
                }
            }
        }
        J::Array(items) => items.iter_mut().for_each(|v| relocated(v, temp)),
        J::Object(items) => items.values_mut().for_each(|v| relocated(v, temp)),
        _ => (),
    }
}

fn protected_admission() -> (tempfile::TempDir, J, J) {
    let temp = util::fs::test_tempdir();
    let mut after = queue();
    relocated(&mut after, temp.path());
    let record = entry(&mut after).clone();
    let root = PathBuf::from(record["from"]["runtime"].as_str().unwrap());
    let guard = root.join("guard");
    util::ensure_private_dir(root.parent().unwrap(), true).unwrap();
    for path in [&root, &guard] {
        util::protected::runtime_namespace(path).unwrap();
        util::atomic_write_json(&path.join("lifecycle-owner.json"), &json!(owner())).unwrap();
    }
    let config = &record["configuration"];
    util::atomic_write_json(
        &root.join("runtime.json"),
        &json!({"root":root,"instance":owner().instance_id,"lifecycle_owner":owner(),
        "adapter":"codex","configuration":"upstream","model":config["model"],"cwd":config["cwd"],
        "role_models":config["role_models"],"upstream_socket":config["upstream_endpoint"],"approval_policy":config["approval"],"driver_binary":config["driver"]}),
    )
    .unwrap();
    let mut state = record["from"]["scope"].clone();
    state.as_object_mut().unwrap().remove("codex_runtime");
    state.as_object_mut().unwrap().extend(
        record["from"]["scope"]["codex_runtime"]
            .as_object()
            .unwrap()
            .clone(),
    );
    state["receiver_kind"] = json!("task-runtime");
    state["control_protocol"] = json!("queue-tool-v2");
    state["approval_policy"] = config["approval"].clone();
    util::atomic_write_json(&guard.join("state.json"), &state).unwrap();
    util::atomic_write_json(
        &root.join("receiver.json"),
        &json!({"phase":"bound","guard_dir":guard,"state":state}),
    )
    .unwrap();
    util::atomic_write_json(
        &root.join("upstream-session.json"),
        &json!({"phase":"created","session":record["from"]["session"],
        "session_configuration":config["business_session_configuration"]}),
    )
    .unwrap();
    let mut before = after.clone();
    before["tasks"]["parent"][KEY] = J::Null;
    (temp, before, after)
}

#[test]
fn first_save_checks_protected_original_runtime_and_session_not_just_scope_json() {
    let (_temp, before, after) = protected_admission();
    validate_queue(&after).unwrap();
    validate_save(&before, &after).unwrap();
    let root = PathBuf::from(
        after["tasks"]["parent"][KEY]["records"][0]["from"]["runtime"]
            .as_str()
            .unwrap(),
    );
    let original = util::read_state_json(&root, "runtime.json").unwrap();
    let mut wrong = original.clone();
    wrong["lifecycle_owner"]["instance_id"] = json!("foreign");
    util::atomic_write_json(&root.join("runtime.json"), &wrong).unwrap();
    assert!(validate_save(&before, &after).is_err());
    util::atomic_write_json(&root.join("runtime.json"), &original).unwrap();
    let mut session = util::read_state_json(&root, "upstream-session.json").unwrap();
    session["session_configuration"]["approvalPolicy"] = json!("never");
    util::atomic_write_json(&root.join("upstream-session.json"), &session).unwrap();
    assert!(validate_save(&before, &after).is_err());
    // Loading the already anchored history does not depend on old live files.
    validate_queue(&after).unwrap();
}

#[test]
fn first_save_checks_the_original_recovery_namespace_and_context_authority() {
    let (_temp, before, mut after) = protected_admission();
    let record = entry(&mut after);
    let from = record["from"].clone();
    let root = PathBuf::from(from["runtime"].as_str().unwrap());
    let observer = root.join("recovery-observer");
    record["configuration"]["role_models"]["recovery"] = model();
    record["recovery"] = json!({"observer":observer,"origin_coordinator":root,"phase":"pending",
        "from_target":{"generation":0,"runtime":root,"scope":from["scope"],"transaction_id":null},
        "to_target":{"generation":1,"runtime":record["to"]["runtime"],
        "scope":record["to"]["scope"],"transaction_id":null}});
    let mut runtime = util::read_state_json(&root, "runtime.json").unwrap();
    runtime["role_models"] = record["configuration"]["role_models"].clone();
    util::atomic_write_json(&root.join("runtime.json"), &runtime).unwrap();
    validate_queue(&after).unwrap();
    assert!(validate_save(&before, &after).is_err());
    util::protected::runtime_namespace(&observer).unwrap();
    util::atomic_write_json(&observer.join("lifecycle-owner.json"), &json!(owner())).unwrap();
    let context = json!({"owner":owner(),"observer":observer,"coordinator":root,
        "runtime_parent":root.parent(),"runtime":runtime});
    util::atomic_write_json(&observer.join("observer.json"), &context).unwrap();
    validate_save(&before, &after).unwrap();
    for (field, value) in [
        ("owner", json!({"instance_id":"foreign"})),
        ("observer", json!(root.join("other-observer"))),
        ("coordinator", json!(root.join("other-runtime"))),
        ("runtime", json!({"root":root.join("other-runtime")})),
    ] {
        let mut foreign = context.clone();
        foreign[field] = value;
        util::atomic_write_json(&observer.join("observer.json"), &foreign).unwrap();
        assert!(validate_save(&before, &after).is_err(), "{field}");
    }
    // The persisted history remains readable without trusting a live observer.
    validate_queue(&after).unwrap();
}
