//! Focused offline coverage for the refactor group E functions.

use herdr_flow::model::{self, J};
use herdr_flow::progress;
use herdr_flow::runtime;
use serde_json::{json, Value};
use tempfile::TempDir;

const SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const SESSION: &str = "11111111-1111-1111-1111-111111111111";
const SESSION2: &str = "22222222-2222-2222-2222-222222222222";

fn wait(id: &str, assignment: &str) -> J {
    json!({
        "id": id,
        "task": "task1",
        "epoch": 1,
        "assignment": assignment,
        "candidate": SHA,
        "owner": "dev",
        "condition": "result",
        "kind": "signal",
        "closed": false,
        "producer_session": SESSION2
    })
}

fn base_queue() -> J {
    json!({
        "version": 1,
        "id": "queue1",
        "guard_dir": Value::Null,
        "receiver": Value::Null,
        "notify": true,
        "stopped": false,
        "waits": {"w1": wait("w1", "a1")},
        "events": {},
        "deliveries": {},
        "active": Value::Null,
        "tasks": {}
    })
}

fn task_spec() -> J {
    json!({
        "task": "task1",
        "wait": "w1",
        "goal": "one bounded step",
        "apply_timeout_seconds": 10,
        "acceptance": [
            {"id": "c1", "description": "first check"},
            {"id": "c2", "description": "second check", "required": false}
        ],
        "authorization": [
            {"id": "develop", "action": "development", "source": "user request"}
        ]
    })
}

fn result_task(assignment: &str, phase: &str, pending: Value) -> J {
    json!({
        "task": "task1",
        "epoch": 1,
        "assignment": assignment,
        "candidate": SHA,
        "owner": "dev",
        "state_version": 1,
        "phase": phase,
        "control": "running",
        "wait": "w1",
        "pending": pending,
        "next_action": {"kind": "wait", "wait": "w1"},
        "acceptance": {"c1": {"description": "first check", "required": true, "evidence": null}},
        "authorization": {"develop": {"action": "development", "source": "user request"}},
        "pending_findings": [],
        "findings": {},
        "operations": {},
        "history": [],
        "apply_timeout": 600.0,
        "writer": Value::Null
    })
}

fn result_event(id: &str, assignment: &str) -> J {
    json!({
        "id": id,
        "wait": "w1",
        "task": "task1",
        "epoch": 1,
        "owner": "dev",
        "candidate": SHA,
        "binding": {
            "task": "task1",
            "epoch": 1,
            "assignment": assignment,
            "candidate": SHA
        },
        "acknowledged": true,
        "archived": false,
        "apply_deadline": 1000.0
    })
}

fn apply_queue(successor_assignment: &str) -> J {
    json!({
        "version": 1,
        "id": "queue1",
        "guard_dir": Value::Null,
        "receiver": Value::Null,
        "notify": true,
        "stopped": false,
        "waits": {"w2": wait("w2", successor_assignment)},
        "events": {"e1": result_event("e1", "a1")},
        "deliveries": {},
        "active": Value::Null,
        "tasks": {"task1": result_task("a1", "result_pending", json!(["e1"]))}
    })
}

fn apply_spec(next_action: J) -> J {
    json!({
        "task": "task1",
        "state_version": 1,
        "event": "e1",
        "acceptance": {"c1": "/evidence/first"},
        "next_action": next_action
    })
}

fn dispatch_action() -> J {
    json!({
        "kind": "dispatch",
        "operation": "op2",
        "assignment": "a2",
        "session": SESSION2,
        "owner": "dev2",
        "instruction": "continue with the bounded successor",
        "action": "development",
        "authorization": "develop",
        "wait": "w2",
        "accept_timeout_seconds": 20,
        "writes": false
    })
}

#[test]
fn workspace_conflicts_cover_lane_writer_and_self_bypass() {
    let mut queue = base_queue();
    queue["tasks"] = json!({
        "task1": {
            "task": "task1",
            "lane": {"workspace_root": "/tmp/lane-root"},
            "writer": {"workspace": "/tmp/writer-root"}
        }
    });
    assert!(
        progress::reserve_writer(&queue, "/tmp/lane-root/child", "a2", SESSION, "task2").is_err()
    );
    assert!(
        progress::reserve_writer(&queue, "/tmp/writer-root/child", "a2", SESSION, "task2").is_err()
    );
    let writer = progress::reserve_writer(&queue, "/tmp/other", "a2", SESSION, "task2").unwrap();
    assert_eq!(model::string(&writer, "assignment").as_deref(), Some("a2"));
    // A task never conflicts with itself.
    let own = progress::reserve_writer(&queue, "/tmp/lane-root/child", "a2", SESSION, "task1");
    assert!(own.is_ok());
}

#[test]
fn register_builds_task_and_validates_acceptance() {
    let mut queue = base_queue();
    let spec = task_spec();
    let task = progress::register(&mut queue, &spec, 100.0).unwrap();
    assert_eq!(model::string(&task, "phase").as_deref(), Some("waiting"));
    assert_eq!(model::string(&task, "task").as_deref(), Some("task1"));
    assert_eq!(model::string(&task, "assignment").as_deref(), Some("a1"));
    assert_eq!(
        model::get(&task, "acceptance").and_then(|v| v.get("c1")),
        Some(&json!({
            "description": "first check",
            "required": true,
            "evidence": null
        }))
    );
    assert_eq!(model::number(&task, "apply_timeout"), Some(10.0));
    assert!(model::dig(&queue, &["tasks", "task1"]).is_some());
    let history = model::array_field(&task, "history").unwrap();
    assert_eq!(
        model::string(&history[0], "action").as_deref(),
        Some("registered")
    );
}

#[test]
fn register_rejects_bad_acceptance_authorization_and_waits() {
    let mut queue = base_queue();
    let mut spec = task_spec();
    spec["acceptance"] = json!([]);
    assert!(progress::register(&mut queue, &spec, 100.0).is_err());

    let mut spec = task_spec();
    spec["acceptance"] = json!([
        {"id": "c1", "description": "one", "required": false}
    ]);
    let error = progress::register(&mut queue, &spec, 100.0).unwrap_err();
    assert!(error
        .message
        .contains("at least one original acceptance criterion"));

    let mut spec = task_spec();
    spec["acceptance"] = json!([
        {"id": "c1", "description": "one"},
        {"id": "c1", "description": "two"}
    ]);
    assert!(progress::register(&mut queue, &spec, 100.0).is_err());

    let mut spec = task_spec();
    spec["acceptance"] = json!([
        {"id": "c1", "description": "one", "required": "yes"}
    ]);
    assert!(progress::register(&mut queue, &spec, 100.0).is_err());

    let mut spec = task_spec();
    spec.as_object_mut().unwrap().remove("acceptance");
    assert!(progress::register(&mut queue, &spec, 100.0).is_err());

    let mut spec = task_spec();
    spec["authorization"] = json!([]);
    assert!(progress::register(&mut queue, &spec, 100.0).is_err());

    let mut spec = task_spec();
    spec["wait"] = json!("missing");
    assert!(progress::register(&mut queue, &spec, 100.0).is_err());

    let spec = task_spec();
    queue["waits"]["w1"]["closed"] = json!(true);
    assert!(progress::register(&mut queue, &spec, 100.0).is_err());

    let mut queue = base_queue();
    queue["events"] = json!({"e0": {"id": "e0", "wait": "w1"}});
    let mut spec = task_spec();
    spec["wait"] = json!("w1");
    assert!(progress::register(&mut queue, &spec, 100.0).is_err());
}

#[test]
fn apply_wait_records_findings_and_archives_superseded_events() {
    let mut queue = apply_queue("a1");
    queue["tasks"]["task1"]["pending"] = json!(["e1", "e2"]);
    queue["tasks"]["task1"]["findings"] = json!({
        "old": {"blocking": false, "evidence": "/old", "clears_when": "never", "resolved": null}
    });
    queue["events"]["e2"] = json!({
        "id": "e2",
        "wait": "w1",
        "task": "task1",
        "epoch": 1,
        "binding": {"task": "task1", "epoch": 1, "assignment": "a1", "candidate": SHA},
        "acknowledged": true,
        "archived": false
    });
    let mut spec = apply_spec(json!({"kind": "wait", "wait": "w2"}));
    spec["findings"] = json!([
        {"id": "f1", "blocking": true, "evidence": "/finding", "clears_when": "fixed"}
    ]);
    spec["resolve_findings"] = json!({"f1": "/finding/resolved", "old": "/old/resolved"});
    spec["archive_events"] = json!(["e2"]);

    let task = progress::apply(&mut queue, &spec, 110.0).unwrap();
    assert_eq!(model::string(&task, "phase").as_deref(), Some("waiting"));
    assert_eq!(model::string(&task, "wait").as_deref(), Some("w2"));
    assert_eq!(model::int(&task, "state_version"), Some(2));
    assert_eq!(
        model::dig_str(&task, &["acceptance", "c1", "evidence", "path"]).as_deref(),
        Some("/evidence/first")
    );
    assert_eq!(
        model::dig_str(&task, &["acceptance", "c1", "evidence", "event"]).as_deref(),
        Some("e1")
    );
    assert_eq!(
        model::dig(&task, &["findings", "f1", "blocking"]),
        Some(&json!(true))
    );
    assert_eq!(
        model::dig_str(&task, &["findings", "f1", "resolved"]).as_deref(),
        Some("/finding/resolved")
    );
    assert_eq!(
        model::dig_str(&task, &["findings", "old", "resolved"]).as_deref(),
        Some("/old/resolved")
    );
    assert_eq!(model::dig(&task, &["pending"]), Some(&json!([])));
    assert!(model::bool_or(
        model::dig(&queue, &["events", "e1"]).unwrap(),
        "applied",
        false
    ));
    assert!(
        model::string(model::dig(&queue, &["events", "e2"]).unwrap(), "archived")
            .unwrap_or_default()
            .contains("e1")
    );
}

#[test]
fn apply_rejects_stale_event_and_unaccounted_pending_results() {
    let mut queue = apply_queue("a1");
    let mut spec = apply_spec(json!({"kind": "wait", "wait": "w2"}));
    spec["state_version"] = json!(99);
    assert!(progress::apply(&mut queue, &spec, 110.0).is_err());

    let mut queue = apply_queue("a1");
    queue["tasks"]["task1"]["pending"] = json!(["e1", "e2"]);
    queue["events"]["e2"] = json!({
        "id": "e2",
        "wait": "w1",
        "task": "task1",
        "epoch": 1,
        "binding": {"task": "task1", "epoch": 1, "assignment": "a1", "candidate": SHA},
        "acknowledged": true,
        "archived": false
    });
    let spec = apply_spec(json!({"kind": "wait", "wait": "w2"}));
    let error = progress::apply(&mut queue, &spec, 110.0).unwrap_err();
    assert!(error.message.contains("archive_events"));
}

#[test]
fn apply_blocked_and_complete_keep_their_guards() {
    let mut queue = apply_queue("a1");
    let task = progress::apply(
        &mut queue,
        &apply_spec(json!({
            "kind": "blocked",
            "owner": "blocked-owner",
            "reason": "dependency",
            "condition": "dependency resolved"
        })),
        110.0,
    )
    .unwrap();
    assert_eq!(model::string(&task, "phase").as_deref(), Some("blocked"));
    assert_eq!(
        model::string(&task, "owner").as_deref(),
        Some("blocked-owner")
    );

    let mut queue = apply_queue("a1");
    let task =
        progress::apply(&mut queue, &apply_spec(json!({"kind": "complete"})), 110.0).unwrap();
    assert_eq!(model::string(&task, "phase").as_deref(), Some("completed"));
    assert_eq!(model::number(&task, "completed_at"), Some(110.0));

    let mut queue = apply_queue("a1");
    let mut spec = apply_spec(json!({"kind": "complete"}));
    spec["acceptance"] = json!({});
    queue["tasks"]["task1"]["acceptance"]["c1"]["evidence"] = Value::Null;
    let error = progress::apply(&mut queue, &spec, 110.0).unwrap_err();
    assert!(error.message.contains("required acceptance"));

    let mut queue = apply_queue("a1");
    queue["tasks"]["task1"]["writer"] = json!({"workspace": "/tmp/work"});
    let error =
        progress::apply(&mut queue, &apply_spec(json!({"kind": "complete"})), 110.0).unwrap_err();
    assert!(error.message.contains("release the active writer"));
}

#[test]
fn apply_dispatch_prepares_begin_sent_and_accept() {
    let mut queue = apply_queue("a2");
    let spec = apply_spec(dispatch_action());
    let task = progress::apply(&mut queue, &spec, 120.0).unwrap();
    assert_eq!(
        model::string(&task, "phase").as_deref(),
        Some("next_pending")
    );
    assert_eq!(model::string(&task, "assignment").as_deref(), Some("a2"));
    assert_eq!(model::string(&task, "owner").as_deref(), Some("dev2"));
    assert_eq!(model::string(&task, "wait").as_deref(), Some("w2"));
    assert_eq!(model::int(&task, "state_version"), Some(2));
    assert_eq!(
        model::dig_str(&task, &["operations", "op2", "phase"]).as_deref(),
        Some("prepared")
    );
    assert_eq!(
        model::dig_f64(&task, &["operations", "op2", "deadline"]),
        Some(140.0)
    );
    assert!(model::dig(&task, &["operations", "op2", "intent", "session"]).is_some());

    let operation = progress::begin(
        &mut queue,
        Some(&json!("task1")),
        Some(&json!(2)),
        Some(&json!("op2")),
        125.0,
    )
    .unwrap();
    assert_eq!(
        model::string(&operation, "phase").as_deref(),
        Some("sending")
    );
    assert_eq!(model::number(&operation, "started_at"), Some(125.0));

    let sent = progress::sent(
        &mut queue,
        &json!({
            "task": "task1",
            "state_version": 3,
            "operation": "op2",
            "response": "rpc-1"
        }),
        126.0,
    )
    .unwrap();
    assert_eq!(
        model::dig_str(&sent, &["operations", "op2", "phase"]).as_deref(),
        Some("submitted")
    );
    assert_eq!(
        model::dig_str(&sent, &["operations", "op2", "response"]).as_deref(),
        Some("rpc-1")
    );

    let accepted = progress::accept(
        &mut queue,
        &json!({
            "task": "task1",
            "state_version": 4,
            "operation": "op2",
            "assignment": "a2",
            "evidence": "takeover evidence"
        }),
        SESSION2,
        127.0,
    )
    .unwrap();
    assert_eq!(
        model::dig_str(&accepted, &["operations", "op2", "phase"]).as_deref(),
        Some("accepted")
    );
    assert_eq!(
        model::string(&accepted, "phase").as_deref(),
        Some("executing")
    );
    assert_eq!(model::int(&accepted, "state_version"), Some(5));
}

#[test]
fn dispatch_and_takeover_validation_rejects_wrong_inputs() {
    let mut queue = apply_queue("a2");
    let mut action = dispatch_action();
    action["authorization"] = json!("missing");
    assert!(progress::apply(&mut queue, &apply_spec(action), 120.0).is_err());

    let mut queue = apply_queue("a2");
    let mut action = dispatch_action();
    action["assignment"] = json!("a1");
    assert!(progress::apply(&mut queue, &apply_spec(action), 120.0).is_err());

    let mut queue = apply_queue("a2");
    let mut action = dispatch_action();
    action["writes"] = json!("yes");
    assert!(progress::apply(&mut queue, &apply_spec(action), 120.0).is_err());

    let mut queue = apply_queue("a2");
    queue["tasks"]["task1"]["writer"] = json!({"workspace": "/tmp/work"});
    assert!(progress::apply(&mut queue, &apply_spec(dispatch_action()), 120.0).is_err());

    let mut queue = apply_queue("a2");
    queue["events"]["e2"] = json!({"id": "e2", "wait": "w2"});
    assert!(progress::apply(&mut queue, &apply_spec(dispatch_action()), 120.0).is_err());

    let mut queue = apply_queue("a2");
    queue["tasks"]["task1"]["operations"] = json!({
        "op2": {"assignment": "a9", "phase": "prepared"}
    });
    assert!(progress::apply(&mut queue, &apply_spec(dispatch_action()), 120.0).is_err());

    let mut queue = apply_queue("a2");
    let mut action = dispatch_action();
    action.as_object_mut().unwrap().remove("instruction");
    assert!(progress::apply(&mut queue, &apply_spec(action), 120.0).is_err());

    let mut queue = apply_queue("a2");
    let mut spec = apply_spec(dispatch_action());
    spec["next_action"]["writes"] = json!(true);
    spec["next_action"]["workspace"] = Value::Null;
    assert!(progress::apply(&mut queue, &spec, 120.0).is_err());
}

#[test]
fn control_resume_and_release_preserve_state_rules() {
    let mut queue = base_queue();
    progress::register(&mut queue, &task_spec(), 100.0).unwrap();
    let paused = progress::control(
        &mut queue,
        Some(&json!("task1")),
        Some(&json!(1)),
        Some(&json!("paused")),
        Some(&json!("operator")),
        105.0,
    )
    .unwrap();
    assert_eq!(model::string(&paused, "control").as_deref(), Some("paused"));
    assert_eq!(
        model::string(&paused, "control_reason").as_deref(),
        Some("operator")
    );
    assert!(progress::control(
        &mut queue,
        Some(&json!("task1")),
        Some(&json!(2)),
        Some(&json!("bogus")),
        Some(&json!("operator")),
        106.0,
    )
    .is_err());

    // Resume only accepts a blocked running task with no pending result.
    let mut queue = base_queue();
    queue["tasks"] = json!({"task1": result_task("a1", "blocked", json!([]))});
    queue["waits"]["w3"] = wait("w3", "a1");
    let resumed = progress::resume(
        &mut queue,
        &json!({
            "task": "task1",
            "state_version": 1,
            "wait": "w3",
            "evidence": "recovered"
        }),
        130.0,
    )
    .unwrap();
    assert_eq!(model::string(&resumed, "phase").as_deref(), Some("waiting"));
    assert_eq!(model::string(&resumed, "wait").as_deref(), Some("w3"));
    assert_eq!(
        model::dig_str(&resumed, &["next_action", "kind"]).as_deref(),
        Some("wait")
    );

    let mut queue = base_queue();
    queue["tasks"] = json!({"task1": result_task("a1", "blocked", json!(["e1"]))});
    assert!(progress::resume(
        &mut queue,
        &json!({
            "task": "task1",
            "state_version": 1,
            "wait": "w1",
            "evidence": "recovered"
        }),
        130.0,
    )
    .is_err());

    let mut queue = base_queue();
    queue["tasks"] = json!({
        "task1": result_task("a1", "blocked", json!([]))
    });
    queue["tasks"]["task1"]["writer"] = json!({"workspace": "/tmp/work"});
    let released = progress::release(
        &mut queue,
        &json!({
            "task": "task1",
            "state_version": 1,
            "evidence": "writer inspected"
        }),
        140.0,
    )
    .unwrap();
    assert!(model::get(&released, "writer").unwrap().is_null());
    let history = model::array_field(&released, "history").unwrap();
    assert_eq!(
        model::string(&history[0], "action").as_deref(),
        Some("writer_released")
    );

    let mut queue = base_queue();
    queue["tasks"] = json!({"task1": result_task("a1", "executing", json!([]))});
    assert!(progress::release(
        &mut queue,
        &json!({
            "task": "task1",
            "state_version": 1,
            "evidence": "writer inspected"
        }),
        140.0,
    )
    .is_err());
}

#[test]
fn stalls_emit_idempotent_progress_events_for_both_phases() {
    let mut queue = base_queue();
    queue["tasks"] = json!({"task1": result_task("a1", "result_pending", json!(["e1"]))});
    queue["events"]["e1"] = json!({
        "id": "e1",
        "wait": "w1",
        "task": "task1",
        "apply_deadline": 50.0
    });
    queue["tasks"]["task1"]["history"] = json!([]);
    progress::stalls(&mut queue, 100.0).unwrap();
    let events = model::get(&queue, "events")
        .and_then(Value::as_object)
        .unwrap();
    let stall = events
        .values()
        .find(|event| model::string(event, "kind").as_deref() == Some("progress_stall"))
        .unwrap();
    assert_eq!(
        model::string(stall, "reason").as_deref(),
        Some("result_unapplied")
    );
    assert_eq!(model::string(stall, "task").as_deref(), Some("task1"));
    assert_eq!(
        model::dig_str(stall, &["binding", "assignment"]).as_deref(),
        Some("a1")
    );
    let event_id = model::string(stall, "id").unwrap();
    assert!(progress::current_event(&queue, stall));
    progress::stalls(&mut queue, 101.0).unwrap();
    assert_eq!(
        model::get(&queue, "events")
            .and_then(Value::as_object)
            .map(|events| events.len()),
        Some(2)
    );
    assert!(model::dig(&queue, &["events", &event_id]).is_some());

    let mut queue = base_queue();
    queue["tasks"] = json!({
        "task1": result_task("a1", "next_pending", json!([])),
        "task2": result_task("a1", "completed", json!([]))
    });
    queue["tasks"]["task1"]["next_action"] = json!({"kind": "dispatch", "operation": "op1"});
    queue["tasks"]["task1"]["operations"] = json!({
        "op1": {"id": "op1", "phase": "prepared", "deadline": 60.0}
    });
    progress::stalls(&mut queue, 100.0).unwrap();
    let stalls = model::get(&queue, "events")
        .and_then(Value::as_object)
        .unwrap()
        .values()
        .filter(|event| model::string(event, "kind").as_deref() == Some("progress_stall"))
        .count();
    assert_eq!(stalls, 1);
}

#[test]
fn current_event_rejects_stale_bindings_and_archived_events() {
    let binding = json!({"task": "task1", "epoch": 1, "assignment": "a1", "candidate": SHA});
    let mut queue = base_queue();
    queue["tasks"] = json!({"task1": result_task("a1", "result_pending", json!(["e1"]))});
    let event = json!({
        "id": "e1",
        "task": "task1",
        "binding": binding,
        "acknowledged": true,
        "archived": false
    });
    assert!(progress::current_event(&queue, &event));
    let mut archived = event.clone();
    archived["archived"] = json!(true);
    assert!(!progress::current_event(&queue, &archived));
    let mut stale = event.clone();
    stale["binding"]["assignment"] = json!("other");
    assert!(!progress::current_event(&queue, &stale));
    let mut no_task = event;
    no_task["task"] = json!("missing");
    assert!(!progress::current_event(&queue, &no_task));
}

#[test]
fn diagnose_and_diagnostics_classify_each_phase() {
    let mut queue = base_queue();
    queue["waits"] = json!({"w1": wait("w1", "a1")});
    queue["tasks"] = json!({
        "task1": result_task("a1", "waiting", json!([])),
        "task2": result_task("a1", "blocked", json!([])),
        "task3": result_task("a1", "completed", json!([]))
    });
    queue["tasks"]["task2"]["next_action"] = json!({
        "kind": "blocked",
        "owner": "dev",
        "reason": "dependency",
        "condition": "dependency resolved"
    });
    let waiting = progress::diagnose(&queue, &queue["tasks"]["task1"], Some(150.0)).unwrap();
    assert_eq!(
        model::string(&waiting, "classification").as_deref(),
        Some("waiting")
    );
    let blocked = progress::diagnose(&queue, &queue["tasks"]["task2"], Some(150.0)).unwrap();
    assert_eq!(
        model::string(&blocked, "classification").as_deref(),
        Some("blocked")
    );
    assert!(model::string(&blocked, "reason")
        .unwrap_or_default()
        .contains("dependency"));
    let completed = progress::diagnose(&queue, &queue["tasks"]["task3"], Some(150.0)).unwrap();
    assert_eq!(
        model::string(&completed, "classification").as_deref(),
        Some("completed")
    );
    let diagnostics = progress::diagnostics(&queue, Some(150.0)).unwrap();
    assert_eq!(
        model::get(&diagnostics, "task1").map(|value| model::string(value, "classification")),
        Some(Some("waiting".to_string()))
    );

    let mut paused = result_task("a1", "executing", json!([]));
    paused["control"] = json!("paused");
    let diagnosis = progress::diagnose(&queue, &paused, None).unwrap();
    assert_eq!(
        model::string(&diagnosis, "classification").as_deref(),
        Some("paused")
    );
    paused["control"] = json!("cancelled");
    let diagnosis = progress::diagnose(&queue, &paused, None).unwrap();
    assert_eq!(
        model::string(&diagnosis, "classification").as_deref(),
        Some("cancelled")
    );
    let unknown = result_task("a1", "mystery", json!([]));
    let diagnosis = progress::diagnose(&queue, &unknown, None).unwrap();
    assert_eq!(
        model::string(&diagnosis, "classification").as_deref(),
        Some("mystery")
    );
}

#[test]
fn diagnose_reports_pending_result_and_successor_deadlines() {
    let mut queue = base_queue();
    queue["tasks"] = json!({"task1": result_task("a1", "result_pending", json!(["e1"]))});
    queue["events"]["e1"] = json!({
        "id": "e1",
        "task": "task1",
        "apply_deadline": 100.0
    });
    let diagnosis = progress::diagnose(&queue, &queue["tasks"]["task1"], Some(200.0)).unwrap();
    assert_eq!(
        model::string(&diagnosis, "classification").as_deref(),
        Some("result_unapplied_overdue")
    );
    assert!(model::string(&diagnosis, "reason")
        .unwrap_or_default()
        .contains("deadline passed"));
    assert!(model::array_field(&diagnosis, "pending_deadlines").is_some());

    let mut task = result_task("a1", "next_pending", json!([]));
    task["next_action"] = json!({"kind": "dispatch", "operation": "op1"});
    task["operations"] = json!({
        "op1": {"id": "op1", "phase": "prepared", "deadline": 100.0}
    });
    let diagnosis = progress::diagnose(&queue, &task, Some(200.0)).unwrap();
    assert_eq!(
        model::string(&diagnosis, "classification").as_deref(),
        Some("successor_prepared_overdue")
    );
    assert_eq!(
        model::dig_str(&diagnosis, &["operation", "id"]).as_deref(),
        Some("op1")
    );
}

#[test]
fn readiness_reports_session_queue_watch_and_guard_gaps() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let queue = json!({
        "version": 1,
        "id": "queue1",
        "guard_dir": Value::Null,
        "receiver": Value::Null,
        "notify": true,
        "stopped": false,
        "waits": {},
        "events": {},
        "deliveries": {}
    });
    herdr_flow::util::atomic_write_json(&root.join("queue.json"), &queue).unwrap();
    let factory = |_: &J| -> herdr_flow::Result<Box<dyn herdr_flow::herdr::ReceiverAdapter>> {
        panic!("factory must not run without a guard")
    };
    let readiness = runtime::readiness(root, None, &factory).unwrap();
    let gaps = model::array_field(&readiness, "gaps").unwrap();
    let text = gaps
        .iter()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>()
        .join("|");
    assert!(text.contains("current receiver session is required"));
    assert!(text.contains("no receiver guard"));
    assert!(text.contains("queue watcher is not healthy"));
    assert!(!model::bool_or(&readiness, "active_wakeup", true));

    // A guard with a mismatched session reports the typed progress message.
    let mut queue = queue;
    queue["guard_dir"] = json!(root.join("guard").display().to_string());
    queue["receiver"] = json!({"task": "other"});
    herdr_flow::util::atomic_write_json(&root.join("queue.json"), &queue).unwrap();
    herdr_flow::util::atomic_write_json(
        &root.join("guard").join("state.json"),
        &json!({
            "version": 2,
            "task": "task1",
            "role": "coordinator",
            "pane": "w1:p1",
            "terminal": "term1",
            "socket": "/herdr",
            "cwd": root.display().to_string(),
            "session": SESSION,
            "phase": "monitoring",
            "auto_rotate": false,
            "usage": {"tokens": 1, "window": 10},
            "level": "normal"
        }),
    )
    .unwrap();
    let readiness = runtime::readiness(root, Some(SESSION), &factory).unwrap();
    let text = model::array_field(&readiness, "gaps")
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>()
        .join("|");
    assert!(text.contains("guard belongs to another registration/session"));

    // With a matching guard identity, an absent watcher becomes a progress gap.
    let guard_state = herdr_flow::util::read_json(&root.join("guard").join("state.json")).unwrap();
    queue["receiver"] = herdr_flow::guard::scope(&guard_state);
    herdr_flow::util::atomic_write_json(&root.join("queue.json"), &queue).unwrap();
    let readiness = runtime::readiness(root, Some(SESSION), &factory).unwrap();
    let text = model::array_field(&readiness, "gaps")
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>()
        .join("|");
    assert!(text.contains("receiver watcher is not healthy"));

    // A missing state file becomes the generic receiver-unavailable gap.
    queue["receiver"] = Value::Null;
    herdr_flow::util::atomic_write_json(&root.join("queue.json"), &queue).unwrap();
    std::fs::remove_file(root.join("guard").join("state.json")).unwrap();
    let readiness = runtime::readiness(root, Some(SESSION), &factory).unwrap();
    let text = model::array_field(&readiness, "gaps")
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>()
        .join("|");
    assert!(text.contains("receiver state unavailable"));
}

#[test]
fn register_reserves_initial_writer_and_enforces_queue_state() {
    let dir = TempDir::new().unwrap();
    let workspace = dir.path().join("work");
    std::fs::create_dir_all(&workspace).unwrap();
    let mut queue = base_queue();
    let mut spec = task_spec();
    spec["workspace"] = json!(workspace.display().to_string());
    spec["execution_evidence"] = json!("initial execution evidence");
    let task = progress::register(&mut queue, &spec, 100.0).unwrap();
    let resolved = herdr_flow::util::resolve_path(&workspace)
        .display()
        .to_string();
    assert_eq!(
        model::dig_str(&task, &["writer", "workspace"]).as_deref(),
        Some(resolved.as_str())
    );
    assert_eq!(
        model::dig_str(&task, &["writer", "assignment"]).as_deref(),
        Some("a1")
    );
    assert_eq!(
        model::dig_str(&task, &["writer", "session"]).as_deref(),
        Some(SESSION2)
    );
    assert_eq!(
        model::dig_str(&task, &["writer", "evidence"]).as_deref(),
        Some("initial execution evidence")
    );

    let mut stopped = base_queue();
    stopped["stopped"] = json!(true);
    let error = progress::register(&mut stopped, &task_spec(), 1.0).unwrap_err();
    assert!(error.message.contains("queue is stopped"));

    let mut duplicate = base_queue();
    progress::register(&mut duplicate, &task_spec(), 1.0).unwrap();
    assert!(progress::register(&mut duplicate, &task_spec(), 2.0).is_err());
}

#[test]
fn apply_dispatch_can_reserve_an_isolated_writer() {
    let dir = TempDir::new().unwrap();
    let workspace = dir.path().join("successor");
    std::fs::create_dir_all(&workspace).unwrap();
    let mut queue = apply_queue("a2");
    let mut action = dispatch_action();
    action["writes"] = json!(true);
    action["workspace"] = json!(workspace.display().to_string());
    let task = progress::apply(&mut queue, &apply_spec(action), 120.0).unwrap();
    let resolved = herdr_flow::util::resolve_path(&workspace)
        .display()
        .to_string();
    assert_eq!(
        model::dig_str(&task, &["writer", "workspace"]).as_deref(),
        Some(resolved.as_str())
    );
    assert_eq!(
        model::dig_str(&task, &["writer", "assignment"]).as_deref(),
        Some("a2")
    );
    assert_eq!(
        model::dig_str(&task, &["writer", "session"]).as_deref(),
        Some(SESSION2)
    );
}

#[test]
fn apply_validates_archive_events_findings_and_release_evidence() {
    let mut queue = apply_queue("a1");
    let mut spec = apply_spec(json!({"kind": "wait", "wait": "w2"}));
    spec["archive_events"] = json!({});
    let error = progress::apply(&mut queue, &spec, 110.0).unwrap_err();
    assert!(error.message.contains("archive_events"));

    let mut queue = apply_queue("a1");
    let mut spec = apply_spec(json!({"kind": "wait", "wait": "w2"}));
    spec["archive_events"] = json!(["e1", 7]);
    assert!(progress::apply(&mut queue, &spec, 110.0).is_err());

    let mut queue = apply_queue("a1");
    let mut spec = apply_spec(json!({"kind": "wait", "wait": "w2"}));
    spec["findings"] = json!([
        {"id": "f1", "blocking": "yes", "evidence": "/e", "clears_when": "fixed"}
    ]);
    assert!(progress::apply(&mut queue, &spec, 110.0).is_err());

    let mut queue = apply_queue("a1");
    queue["tasks"]["task1"]["findings"] = json!({
        "f1": {"blocking": true, "evidence": "/e", "clears_when": "fixed", "resolved": null}
    });
    let mut spec = apply_spec(json!({"kind": "wait", "wait": "w2"}));
    spec["findings"] = json!([
        {"id": "f1", "blocking": true, "evidence": "/e", "clears_when": "fixed"}
    ]);
    assert!(progress::apply(&mut queue, &spec, 110.0).is_err());

    let mut queue = apply_queue("a1");
    queue["tasks"]["task1"]["writer"] = json!({"workspace": "/tmp/work"});
    let mut spec = apply_spec(json!({"kind": "wait", "wait": "w2"}));
    spec["release_evidence"] = json!("/released");
    let task = progress::apply(&mut queue, &spec, 110.0).unwrap();
    assert!(model::get(&task, "writer").unwrap().is_null());
    assert_eq!(
        model::dig_str(&task, &["last_release", "evidence"]).as_deref(),
        Some("/released")
    );
    assert_eq!(
        model::dig_str(&task, &["last_release", "assignment"]).as_deref(),
        Some("a1")
    );

    let mut queue = apply_queue("a1");
    let mut spec = apply_spec(json!({"kind": "wait", "wait": "w2"}));
    spec.as_object_mut().unwrap().remove("next_action");
    assert!(progress::apply(&mut queue, &spec, 110.0).is_err());
}

#[test]
fn stalls_reports_event_capacity_exhaustion() {
    let mut queue = base_queue();
    queue["tasks"] = json!({"task1": result_task("a1", "result_pending", json!(["e1"]))});
    let mut events = serde_json::Map::new();
    events.insert(
        "e1".to_string(),
        json!({"id": "e1", "wait": "w1", "task": "task1", "apply_deadline": 50.0}),
    );
    for index in 0..9_999 {
        events.insert(format!("filler{index}"), json!({"id": index}));
    }
    queue["events"] = Value::Object(events);
    let error = progress::stalls(&mut queue, 100.0).unwrap_err();
    assert!(error.message.contains("queue event capacity"));
}
