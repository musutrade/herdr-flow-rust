//! Read-only metrics, one-shot command capture and watcher liveness.

pub mod common;

use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::sync::Mutex;

use herdr_flow::herdr::ReceiverAdapter;
use herdr_flow::metrics;
use herdr_flow::model::{self, J};
use herdr_flow::queue;
use herdr_flow::util::{self, FileLock};
use herdr_flow::watch_service;
use serde_json::{json, Value};

const SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

struct Noop {
    _shared: Arc<Mutex<()>>,
}

impl ReceiverAdapter for Noop {
    fn agent(&self) -> herdr_flow::Result<J> {
        Ok(
            json!({"agent_status": "idle", "interactive_ready": true, "agent_session": {"value": null}}),
        )
    }
    fn prompt(&self, _text: &str) -> herdr_flow::Result<String> {
        Ok("turn".to_string())
    }
    fn call(&self, _args: &[String]) -> herdr_flow::Result<J> {
        Ok(json!({}))
    }
}

fn factory() -> impl Fn(&J) -> herdr_flow::Result<Box<dyn ReceiverAdapter>> {
    |_state: &J| {
        Ok(Box::new(Noop {
            _shared: Arc::new(Mutex::new(())),
        }))
    }
}

fn metric_state() -> J {
    let binding = json!({"task": "o/r#1", "epoch": 1, "assignment": "a1", "candidate": SHA});
    json!({
        "id": "queue-1",
        "stopped": false,
        "notify": true,
        "delivery_status": "acknowledged",
        "guard_dir": null,
        "receiver": null,
        "waits": {
            "w1": {"id": "w1", "task": "o/r#1", "kind": "signal", "owner": "dev",
                   "condition": "first result", "registered_at": 101.0, "deadline": 1000.0, "closed": true},
            "w2": {"id": "w2", "task": "o/r#1", "kind": "signal", "owner": "dev",
                   "condition": "second result", "registered_at": 135.0, "deadline": 1000.0, "closed": false}
        },
        "events": {
            "e1": {"id": "e1", "wait": "w1", "task": "o/r#1", "epoch": 1, "outcome": "complete",
                   "observed_at": 110.0, "acknowledged": true, "applied": true, "applied_at": 130.0,
                   "binding": binding},
            "e2": {"id": "e2", "wait": "w2", "task": "o/r#1", "epoch": 1, "outcome": "complete",
                   "observed_at": 150.0, "acknowledged": false, "applied": false},
            "s1": {"id": "s1", "wait": "w1", "task": "o/r#1", "epoch": 1, "kind": "progress_stall",
                   "reason": "result_unapplied", "observed_at": 140.0}
        },
        "deliveries": {
            "d1": {"id": "d1", "events": ["e1"], "phase": "acknowledged", "sent_at": 115.0, "ack_at": 120.0}
        },
        "tasks": {
            "o/r#1": {"task": "o/r#1", "epoch": 1, "assignment": "a1", "candidate": SHA,
                "phase": "completed", "control": "running", "owner": "dev",
                "created_at": 100.0, "completed_at": 200.0, "pending": [],
                "operations": {"op2": {"id": "op2", "assignment": "a2", "phase": "accepted",
                    "prepared_at": 131.0, "started_at": 132.0, "submitted_at": 133.0,
                    "accepted_at": 134.0, "deadline": 1000.0, "intent": {"wait": "w2"}}},
                "history": [
                    {"at": 130.0, "version": 2, "action": "applied", "event": "e1"},
                    {"at": 134.0, "version": 3, "action": "recovery_wait", "wait": "w2"}
                ]}
        }
    })
}

#[test]
fn metrics_report_spans_unknowns_and_open_waits() {
    let tmp = common::tempdir().unwrap();
    util::atomic_write_json(&tmp.path().join("queue.json"), &metric_state()).unwrap();
    let report = metrics::report(tmp.path(), Some(160.0)).unwrap();
    let task = &report["tasks"][0];
    assert_eq!(
        model::dig_f64(task, &["segments", "created_to_first_result", "seconds"]),
        Some(10.0)
    );
    assert_eq!(
        model::dig_f64(task, &["segments", "created_to_completed", "seconds"]),
        Some(100.0)
    );
    let event = model::array_field(task, "events")
        .unwrap()
        .iter()
        .find(|event| model::string(event, "id").as_deref() == Some("e1"))
        .unwrap();
    assert_eq!(
        model::dig_f64(event, &["segments", "ready_to_delivery", "seconds"]),
        Some(5.0)
    );
    assert_eq!(
        model::dig_f64(event, &["segments", "delivery_to_ack", "seconds"]),
        Some(5.0)
    );
    assert_eq!(
        model::dig_f64(event, &["segments", "ack_to_applied", "seconds"]),
        Some(10.0)
    );
    let operation = &task["operations"][0];
    assert_eq!(
        model::dig_f64(operation, &["segments", "prepared_to_accepted", "seconds"]),
        Some(3.0)
    );
    assert_eq!(
        model::dig_f64(
            operation,
            &["segments", "accepted_to_next_result", "seconds"]
        ),
        Some(16.0)
    );
    assert_eq!(
        model::array_field(task, "open_waits")
            .unwrap()
            .iter()
            .filter_map(|wait| model::string(wait, "id"))
            .collect::<Vec<_>>(),
        vec!["w2"]
    );
    assert_eq!(model::dig_int(task, &["counts", "stalls"]), Some(1));
    assert_eq!(
        model::dig_int(task, &["counts", "stall_reasons", "result_unapplied"]),
        Some(1)
    );
    assert_eq!(
        model::dig_int(task, &["recorded_interventions", "recovery_wait"]),
        Some(1)
    );
    let text = metrics::format_report(&report);
    assert!(text.contains("capabilities:"));
    assert!(text.contains("delivery_to_ack"));
    assert!(!text.contains("diagnosis"));
}

#[test]
fn metrics_unknown_and_clock_anomaly_are_not_zero() {
    let tmp = common::tempdir().unwrap();
    let mut state = metric_state();
    state["events"]["e1"]["applied"] = json!(false);
    state["events"]["e1"]
        .as_object_mut()
        .unwrap()
        .remove("applied_at");
    state["deliveries"]["d1"]
        .as_object_mut()
        .unwrap()
        .remove("ack_at");
    util::atomic_write_json(&tmp.path().join("queue.json"), &state).unwrap();
    let report = metrics::report(tmp.path(), Some(160.0)).unwrap();
    let task = &report["tasks"][0];
    let event = model::array_field(task, "events")
        .unwrap()
        .iter()
        .find(|event| model::string(event, "id").as_deref() == Some("e1"))
        .unwrap();
    assert!(
        model::dig(event, &["segments", "delivery_to_ack", "seconds"])
            .unwrap()
            .is_null()
    );
    assert!(model::string(
        model::dig(event, &["segments", "delivery_to_ack"]).unwrap(),
        "reason"
    )
    .unwrap()
    .contains("ACK"));
    assert!(
        model::dig(event, &["segments", "ack_to_applied", "seconds"])
            .unwrap()
            .is_null()
    );
    assert!(model::string(
        model::dig(event, &["segments", "ack_to_applied"]).unwrap(),
        "reason"
    )
    .unwrap()
    .contains("not applied"));
    assert!(model::dig(task, &["segments", "created_to_completed"]).is_some());

    let mut state = metric_state();
    state["events"]["e1"]["applied_at"] = json!(100.0);
    util::atomic_write_json(&tmp.path().join("queue.json"), &state).unwrap();
    let report = metrics::report(tmp.path(), Some(160.0)).unwrap();
    let event = model::array_field(&report["tasks"][0], "events")
        .unwrap()
        .iter()
        .find(|event| model::string(event, "id").as_deref() == Some("e1"))
        .unwrap();
    assert_eq!(
        model::dig_f64(event, &["segments", "ack_to_applied", "seconds"]),
        None
    );
    assert!(model::string(
        model::dig(event, &["segments", "ack_to_applied"]).unwrap(),
        "reason"
    )
    .unwrap()
    .contains("clock anomaly"));

    let mut state = metric_state();
    state["tasks"]["o/r#1"]
        .as_object_mut()
        .unwrap()
        .remove("completed_at");
    util::atomic_write_json(&tmp.path().join("queue.json"), &state).unwrap();
    let report = metrics::report(tmp.path(), Some(160.0)).unwrap();
    let task = &report["tasks"][0];
    assert_eq!(
        model::dig_f64(task, &["segments", "ongoing_since_created", "seconds"]),
        Some(60.0)
    );
    assert_eq!(
        model::dig_int(
            &report,
            &["aggregate", "segments", "ongoing_since_created", "ongoing"]
        ),
        Some(1)
    );
    assert_eq!(
        model::dig_int(
            &report,
            &["aggregate", "segments", "created_to_completed", "unknown"]
        ),
        Some(1)
    );
}

#[test]
fn command_wrapper_captures_once_and_signals_status() {
    let tmp = common::tempdir().unwrap();
    let root = tmp.path().join("queue");
    common::create_dir_all(&root).unwrap();
    queue::init(&root, None, 30).unwrap();
    queue::add(
        &root,
        &json!({
            "id": "build",
            "task": "o/r#1",
            "epoch": 1,
            "owner": "dev",
            "reason": "build",
            "condition": "command exit recorded",
            "kind": "signal",
            "timeout_seconds": 60
        }),
    )
    .unwrap();
    let argv = json!(["/bin/sh", "-c", "echo ok; exit 7"]);
    let code = queue::run_command(&root, "build", &argv, tmp.path().to_str().unwrap()).unwrap();
    assert_eq!(code, 1);
    assert_eq!(
        std::fs::read_to_string(root.join("commands/build/stdout.log"))
            .unwrap()
            .trim(),
        "ok"
    );
    let result = util::read_json(&root.join("commands/build/result.json")).unwrap();
    assert_eq!(model::int(&result, "exit_code"), Some(7));
    let f = factory();
    let state = queue::tick(&root, &f, None).unwrap();
    assert_eq!(
        model::dig_str(&state, &["waits", "build", "outcome"]).as_deref(),
        Some("failed")
    );
    assert!(queue::run_command(
        &root,
        "build",
        &json!(["echo", "again"]),
        tmp.path().to_str().unwrap()
    )
    .is_err());
}

#[test]
fn missing_command_binary_becomes_decision() {
    let tmp = common::tempdir().unwrap();
    let root = tmp.path().join("queue");
    common::create_dir_all(&root).unwrap();
    queue::init(&root, None, 30).unwrap();
    queue::add(
        &root,
        &json!({
            "id": "build",
            "task": "o/r#1",
            "epoch": 1,
            "owner": "dev",
            "reason": "build",
            "condition": "command exit recorded",
            "kind": "signal",
            "timeout_seconds": 60
        }),
    )
    .unwrap();
    let code = queue::run_command(
        &root,
        "build",
        &json!(["/nonexistent/command"]),
        tmp.path().to_str().unwrap(),
    )
    .unwrap();
    assert_eq!(code, 1);
    let f = factory();
    let state = queue::tick(&root, &f, None).unwrap();
    assert_eq!(
        model::dig_str(&state, &["waits", "build", "outcome"]).as_deref(),
        Some("decision")
    );
}

#[test]
fn either_log_open_failure_retains_decision_without_executing_or_replaying() {
    for name in ["stdout.log", "stderr.log"] {
        let tmp = common::tempdir().unwrap();
        let root = tmp.path().join("queue");
        queue::init(&root, None, 30).unwrap();
        queue::add(
            &root,
            &json!({"id":"build","task":"o/r#1","epoch":1,"owner":"dev",
                "reason":"capture failure","condition":"command result recorded",
                "kind":"signal","timeout_seconds":60}),
        )
        .unwrap();
        let folder = root.join("commands/build");
        common::create_dir_all(folder.join(name)).unwrap();
        let marker = tmp.path().join("executed");
        let script = format!("touch {}", util::shell_quote(marker.to_str().unwrap()));
        let argv = json!(["/bin/sh", "-c", script]);
        assert_eq!(
            queue::run_command(&root, "build", &argv, tmp.path().to_str().unwrap()).unwrap(),
            1
        );
        let record = util::read_json(&folder.join("result.json")).unwrap();
        assert_eq!(record["error"], "LogError");
        assert!(!record["error_detail"].as_str().unwrap().is_empty());
        assert!(record["finished_at"].is_number());
        assert!(folder.join("invocation.json").is_file());
        assert!(!folder.join("process.json").exists());
        assert!(!marker.exists());
        let other = if name == "stdout.log" {
            "stderr.log"
        } else {
            "stdout.log"
        };
        assert!(folder.join(other).is_file());
        let state = queue::tick(&root, &factory(), None).unwrap();
        assert_eq!(state["waits"]["build"]["outcome"], "decision");
        assert!(queue::run_command(&root, "build", &argv, tmp.path().to_str().unwrap()).is_err());
        assert!(!marker.exists());
    }
}

fn healthy_watcher(dir: &Path) -> FileLock {
    common::create_dir_all(dir).unwrap();
    let pid = std::process::id();
    util::atomic_write_json(
        &dir.join("watch.json"),
        &json!({
            "pid": pid,
            "start_ticks": util::process_start_ticks(pid).unwrap_or_default(),
            "started_at": util::now(),
            "heartbeat_at": util::now(),
            "max_age": 600,
            "status": "running"
        }),
    )
    .unwrap();
    FileLock::acquire(&dir.join("watch.lock"), false)
        .unwrap()
        .expect("lock")
}

#[test]
fn watcher_health_distinguishes_stopped_heartbeat_and_lock() {
    let tmp = common::tempdir().unwrap();
    let root = tmp.path().join("watch");
    let lock = healthy_watcher(&root);
    assert!(model::bool_or(
        &watch_service::health(&root),
        "healthy",
        false
    ));
    drop(lock);
    let health = watch_service::health(&root);
    assert!(!model::bool_or(&health, "healthy", true));
    assert!(model::string(&health, "reason")
        .unwrap_or_default()
        .contains("lock"));
    let state = util::read_json(&root.join("watch.json")).unwrap();
    util::atomic_write_json(
        &root.join("watch.json"),
        &json!({
            "pid": state["pid"].clone(),
            "start_ticks": state["start_ticks"].clone(),
            "started_at": 0,
            "heartbeat_at": 0,
            "max_age": 1,
            "status": "running"
        }),
    )
    .unwrap();
    let health = watch_service::health(&root);
    assert!(!model::bool_or(&health, "healthy", true));
    assert!(model::string(&health, "reason")
        .unwrap_or_default()
        .contains("heartbeat"));
}

#[test]
fn command_uses_the_configured_cwd() {
    let tmp = common::tempdir().unwrap();
    let root = tmp.path().join("queue");
    common::create_dir_all(&root).unwrap();
    queue::init(&root, None, 30).unwrap();
    queue::add(
        &root,
        &json!({
            "id": "pwd",
            "task": "o/r#1",
            "epoch": 1,
            "owner": "dev",
            "reason": "pwd",
            "condition": "command exit recorded",
            "kind": "signal",
            "timeout_seconds": 60
        }),
    )
    .unwrap();
    let cwd = tmp.path().join("work");
    common::create_dir_all(&cwd).unwrap();
    let code = queue::run_command(
        &root,
        "pwd",
        &json!(["/bin/sh", "-c", "pwd"]),
        cwd.to_str().unwrap(),
    )
    .unwrap();
    assert_eq!(code, 0);
    let output = std::fs::read_to_string(root.join("commands/pwd/stdout.log")).unwrap();
    assert_eq!(
        output.trim(),
        cwd.canonicalize().unwrap().display().to_string()
    );
    let _ = Value::Null;
    let _ = Command::new("true").status();
}
