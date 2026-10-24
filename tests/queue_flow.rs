//! End-to-end queue, delivery, task and progress behavior over real files.

pub mod common;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use herdr_flow::herdr::ReceiverAdapter;
use herdr_flow::model::{self, J};
use herdr_flow::queue;
use herdr_flow::util::{self, FileLock};
use serde_json::{json, Value};
use tempfile::TempDir;

const SESSION: &str = "11111111-1111-1111-1111-111111111111";
const SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

#[test]
fn cli_watch_once_handles_idle_and_busy_queues() {
    let temp = common::tempdir().unwrap();
    let root = temp.path().join("queue");
    queue::init(&root, None, 30).unwrap();
    let run = || {
        std::process::Command::new(env!("CARGO_BIN_EXE_herdr-flow"))
            .args([
                "queue",
                "watch",
                "--state-dir",
                root.to_str().unwrap(),
                "--once",
            ])
            .output()
            .unwrap()
    };
    let output = run();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let lock = queue::locked(&root, false).unwrap();
    let output = run();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("busy"));
    drop(lock);
    assert!(run().status.success());
}

#[derive(Clone)]
struct AgentState {
    messages: Vec<String>,
    notices: Vec<Vec<String>>,
    status: String,
    session: Option<String>,
    fail: bool,
}

struct FakeAgent {
    state: Arc<Mutex<AgentState>>,
}

impl ReceiverAdapter for FakeAgent {
    fn agent(&self) -> herdr_flow::Result<J> {
        let state = self.state.lock().unwrap();
        Ok(json!({
            "agent_status": state.status,
            "interactive_ready": true,
            "agent_session": {"value": state.session}
        }))
    }

    fn prompt(&self, text: &str) -> herdr_flow::Result<String> {
        let mut state = self.state.lock().unwrap();
        state.messages.push(text.to_string());
        if state.fail {
            return Err(herdr_flow::FlowError::guard("ambiguous send"));
        }
        Ok("turn-one".to_string())
    }

    fn call(&self, args: &[String]) -> herdr_flow::Result<J> {
        let mut state = self.state.lock().unwrap();
        if state.fail {
            return Err(herdr_flow::FlowError::guard("ambiguous notification"));
        }
        state.notices.push(args.to_vec());
        Ok(json!({"ok": true}))
    }
}

fn factory(
    state: Arc<Mutex<AgentState>>,
) -> impl Fn(&J) -> herdr_flow::Result<Box<dyn ReceiverAdapter>> {
    move |_state: &J| {
        Ok(Box::new(FakeAgent {
            state: state.clone(),
        }))
    }
}

fn healthy_watcher(dir: &Path) -> FileLock {
    common::create_dir_all(dir).unwrap();
    let pid = std::process::id();
    let ticks = util::process_start_ticks(pid).unwrap_or_default();
    util::atomic_write_json(
        &dir.join("watch.json"),
        &json!({
            "pid": pid,
            "start_ticks": ticks,
            "started_at": util::now(),
            "heartbeat_at": util::now(),
            "max_age": 600,
            "status": "running"
        }),
    )
    .unwrap();
    FileLock::acquire(&dir.join("watch.lock"), false)
        .unwrap()
        .expect("watcher lock")
}

struct Harness {
    _tmp: TempDir,
    base: PathBuf,
    guard: PathBuf,
    queue: PathBuf,
    _guard_watch: FileLock,
    _queue_watch: FileLock,
    agent: Arc<Mutex<AgentState>>,
}

impl Harness {
    fn new() -> Self {
        let tmp = common::tempdir().unwrap();
        let base = tmp.path().to_path_buf();
        let guard = base.join("guard");
        let queue_dir = base.join("queue");
        common::create_dir_all(&guard).unwrap();
        common::create_dir_all(&queue_dir).unwrap();
        common::create_dir_all(base.join("repo")).unwrap();
        let guard_state = json!({
            "version": 2,
            "task": "o/r#1",
            "role": "coordinator",
            "pane": "w1:p1",
            "terminal": "term1",
            "socket": "/herdr",
            "cwd": base.join("repo").display().to_string(),
            "session": SESSION,
            "phase": "monitoring",
            "auto_rotate": false,
            "usage": {"tokens": 100, "window": 10000},
            "level": "normal"
        });
        util::atomic_write_json(&guard.join("state.json"), &guard_state).unwrap();
        let guard_watch = healthy_watcher(&guard);
        let queue_watch = healthy_watcher(&queue_dir);
        queue::init(&queue_dir, Some(&guard), 30).unwrap();
        let agent = Arc::new(Mutex::new(AgentState {
            messages: Vec::new(),
            notices: Vec::new(),
            status: "idle".to_string(),
            session: Some(SESSION.to_string()),
            fail: false,
        }));
        Self {
            _tmp: tmp,
            base,
            guard,
            queue: queue_dir,
            _guard_watch: guard_watch,
            _queue_watch: queue_watch,
            agent,
        }
    }

    fn enable(&self) {
        let factory = factory(self.agent.clone());
        let mut s = queue::load_queue(&self.queue).unwrap();
        queue::enable(&self.queue, &mut s, &factory).unwrap();
    }

    fn add(&self, id: &str, assignment: Option<&str>, extra: Value) {
        let mut spec = json!({
            "id": id,
            "task": "o/r#1",
            "epoch": 1,
            "owner": "dev",
            "reason": "step result",
            "condition": "declared acceptance checked",
            "kind": "signal",
            "timeout_seconds": 1000
        });
        if let Some(assignment) = assignment {
            spec["assignment"] = json!(assignment);
            spec["candidate"] = json!(SHA);
        }
        if let Some(extra) = extra.as_object() {
            for (key, value) in extra {
                spec[key] = value.clone();
            }
        }
        queue::add(&self.queue, &spec).unwrap();
    }

    fn tick(&self, now: Option<f64>) -> J {
        let factory = factory(self.agent.clone());
        queue::tick(&self.queue, &factory, now).unwrap()
    }

    fn task_spec(&self) -> J {
        json!({
            "claim":{"version":1,"kind":"independent","admission_evidence":"/evidence/admission"},
            "task": "o/r#1",
            "wait": "step1",
            "goal": "two accepted steps",
            "apply_timeout_seconds": 10,
            "acceptance": [
                {"id": "first", "description": "first step checked"},
                {"id": "second", "description": "second step checked"}
            ],
            "authorization": [
                {"id": "develop", "action": "development", "source": "user request"}
            ]
        })
    }
}

fn event_for(s: &J, wait: &str) -> String {
    model::get(s, "events")
        .and_then(Value::as_object)
        .and_then(|events| {
            events.values().find(|event| {
                model::string(event, "wait").as_deref() == Some(wait)
                    && model::string(event, "kind").as_deref() != Some("progress_stall")
            })
        })
        .and_then(|event| model::string(event, "id"))
        .expect("event")
}

#[test]
fn validates_wait_inputs_and_queue_location() {
    let h = Harness::new();
    let result = queue::add(
        &h.queue,
        &json!({
            "id": "bad id",
            "task": "o/r#1",
            "epoch": 1,
            "owner": "dev",
            "reason": "x",
            "condition": "y",
            "kind": "signal",
            "timeout_seconds": 10
        }),
    );
    assert!(result.is_err());
    let result = queue::add(
        &h.queue,
        &json!({
            "id": "guard",
            "task": "o/r#1",
            "epoch": 1,
            "owner": "dev",
            "reason": "x",
            "condition": "y",
            "kind": "guard",
            "timeout_seconds": 10
        }),
    );
    assert!(result.is_err());
    let inside = h.base.join("repo").join("queue-inside");
    common::create_dir_all(&inside).unwrap();
    assert!(queue::init(&inside, Some(&h.guard), 30).is_err());
}

#[test]
fn delivery_is_submitted_and_acknowledged_once() {
    let h = Harness::new();
    h.enable();
    h.add("step1", None, json!({}));
    queue::signal(&h.queue, "step1", "complete", "done", None).unwrap();
    let s = h.tick(Some(util::now() + 1.0));
    let active = model::string(&s, "active").expect("active delivery");
    assert_eq!(
        model::dig_str(&s, &["deliveries", &active, "phase"]).as_deref(),
        Some("submitted")
    );
    assert_eq!(h.agent.lock().unwrap().messages.len(), 1);
    let guard_state = util::read_json(&h.guard.join("state.json")).unwrap();
    assert!(model::get(&guard_state, "external_delivery").is_some());
    let s = queue::acknowledge(&h.queue, &active, SESSION).unwrap();
    assert!(model::get(&s, "active").unwrap().is_null());
    let guard_state = util::read_json(&h.guard.join("state.json")).unwrap();
    assert!(model::get(&guard_state, "external_delivery").is_none());
}

#[test]
fn inspected_resolution_preserves_delivery_identity_and_receipt_policy() {
    for resolution in ["received", "not-delivered"] {
        let h = Harness::new();
        h.enable();
        h.add("step1", None, json!({}));
        queue::signal(&h.queue, "step1", "complete", "done", None).unwrap();
        let before = h.tick(Some(util::now() + 1.0));
        let delivery = model::string(&before, "active").unwrap();
        let event = event_for(&before, "step1");
        let reason = format!("inspected {resolution} in the registered pane");

        let after = queue::resolve(&h.queue, &delivery, resolution, &reason).unwrap();
        assert_eq!(
            std::fs::read_to_string(h.queue.join("queue.json")).unwrap(),
            format!("{}\n", serde_json::to_string_pretty(&after).unwrap())
        );
        assert!(after["active"].is_null());
        assert_eq!(after["notify"], false);
        assert_eq!(after["delivery_status"], "resolved_delivery_disabled");
        let record = &after["deliveries"][&delivery];
        assert_eq!(record["id"], delivery);
        assert_eq!(record["session"], SESSION);
        assert_eq!(record["phase"], "resolved");
        assert_eq!(record["resolution"], resolution);
        assert_eq!(record["reason"], reason);
        assert_eq!(
            after["events"][&event]["acknowledged"],
            resolution == "received"
        );
        let guard = util::read_json(&h.guard.join("state.json")).unwrap();
        assert!(guard.get("external_delivery").is_none());
        assert_eq!(h.agent.lock().unwrap().messages.len(), 1);
    }
}

#[test]
fn inspected_resolution_rejects_another_delivery_without_mutating_state() {
    let h = Harness::new();
    h.enable();
    h.add("step1", None, json!({}));
    queue::signal(&h.queue, "step1", "complete", "done", None).unwrap();
    let before = h.tick(Some(util::now() + 1.0));
    let delivery = model::string(&before, "active").unwrap();
    let mut guard = util::read_json(&h.guard.join("state.json")).unwrap();
    guard["external_delivery"]["delivery"] = json!("another-delivery");
    util::atomic_write_json(&h.guard.join("state.json"), &guard).unwrap();
    let queue_bytes = std::fs::read(h.queue.join("queue.json")).unwrap();
    let guard_bytes = std::fs::read(h.guard.join("state.json")).unwrap();

    let error = queue::resolve(&h.queue, &delivery, "received", "inspected pane").unwrap_err();
    assert!(error.message.contains("another delivery"));
    assert_eq!(
        std::fs::read(h.queue.join("queue.json")).unwrap(),
        queue_bytes
    );
    assert_eq!(
        std::fs::read(h.guard.join("state.json")).unwrap(),
        guard_bytes
    );
}

#[test]
fn result_association_is_persisted_before_ack() {
    let h = Harness::new();
    h.enable();
    h.add("step1", Some("assignment1"), json!({}));
    let mut registered = queue::load_queue(&h.queue).unwrap();
    herdr_flow::progress::register(&mut registered, &h.task_spec(), 100.0).unwrap();
    queue::save(&h.queue, &mut registered).unwrap();
    queue::signal(
        &h.queue,
        "step1",
        "complete",
        "result published",
        Some("/evidence"),
    )
    .unwrap();
    let s = h.tick(Some(100.0));
    let event_id = event_for(&s, "step1");
    let disk = queue::load_queue(&h.queue).unwrap();
    let event = model::dig(&disk, &["events", &event_id]).unwrap();
    assert!(!model::bool_or(event, "acknowledged", true));
    assert_eq!(model::number(event, "apply_deadline"), Some(110.0));
    let pending =
        model::array_field(model::dig(&disk, &["tasks", "o/r#1"]).unwrap(), "pending").unwrap();
    assert!(pending
        .iter()
        .any(|value| value.as_str() == Some(event_id.as_str())));
}

#[test]
fn task_registration_apply_and_completion() {
    let h = Harness::new();
    h.enable();
    h.add("step1", Some("assignment1"), json!({}));
    let mut s = queue::load_queue(&h.queue).unwrap();
    herdr_flow::progress::register(&mut s, &h.task_spec(), 100.0).unwrap();
    queue::save(&h.queue, &mut s).unwrap();
    queue::signal(
        &h.queue,
        "step1",
        "complete",
        "result published",
        Some("/evidence"),
    )
    .unwrap();
    let s = h.tick(Some(110.0));
    let event_id = event_for(&s, "step1");
    let active = model::string(&s, "active").expect("delivery");
    let s = queue::acknowledge(&h.queue, &active, SESSION).unwrap();
    let version = model::int(
        model::dig(&s, &["tasks", "o/r#1"]).unwrap(),
        "state_version",
    )
    .unwrap();
    let spec = json!({
        "task": "o/r#1",
        "state_version": version,
        "event": event_id,
        "acceptance": {"first": "/first-result", "second": "/second-result"},
        "next_action": {"kind": "complete"}
    });
    let mut s = s;
    herdr_flow::progress::apply(&mut s, &spec, 115.0).unwrap();
    assert_eq!(
        model::dig_str(&s, &["tasks", "o/r#1", "phase"]).as_deref(),
        Some("completed")
    );
    assert_eq!(
        model::dig_str(
            &s,
            &["tasks", "o/r#1", "acceptance", "first", "evidence", "path"]
        )
        .as_deref(),
        Some("/first-result")
    );
}

#[test]
fn writer_reservation_prevents_overlapping_workspaces() {
    let h = Harness::new();
    let s = queue::load_queue(&h.queue).unwrap();
    let workspace = h.base.join("work-one");
    common::create_dir_all(&workspace).unwrap();
    if model::get(&s, "tasks").is_none() {
        let mut initial = s.clone();
        initial["tasks"] = json!({});
        queue::save(&h.queue, &mut initial).unwrap();
    }
    let mut s = queue::load_queue(&h.queue).unwrap();
    let writer = herdr_flow::progress::reserve_writer(
        &s,
        &workspace.display().to_string(),
        "assignment1",
        SESSION,
        "o/r#1",
    )
    .unwrap();
    s["tasks"]["o/r#1"] = json!({
        "task": "o/r#1",
        "epoch": 1,
        "assignment": "assignment1",
        "candidate": SHA,
        "writer": writer
    });
    assert!(herdr_flow::progress::reserve_writer(
        &s,
        &workspace.join("sub").display().to_string(),
        "assignment2",
        SESSION,
        "o/r#2",
    )
    .is_err());
    let other = h.base.join("work-two");
    common::create_dir_all(&other).unwrap();
    assert!(herdr_flow::progress::reserve_writer(
        &s,
        &other.display().to_string(),
        "assignment2",
        SESSION,
        "o/r#2",
    )
    .is_ok());
}

#[test]
fn worker_guard_fault_becomes_a_decision_event() {
    let h = Harness::new();
    let worker = h.base.join("worker");
    common::create_dir_all(&worker).unwrap();
    let mut state = util::read_json(&h.guard.join("state.json")).unwrap();
    state["task"] = json!("o/r#1");
    state["role"] = json!("review");
    state["pane"] = json!("w1:p2");
    state["terminal"] = json!("term2");
    util::atomic_write_json(&worker.join("state.json"), &state).unwrap();
    let _worker_watch = healthy_watcher(&worker);
    h.enable();
    h.add(
        "step1",
        None,
        json!({
            "kind": "guard",
            "source": worker.display().to_string(),
            "guard_identity": herdr_flow::guard::scope(&state)
        }),
    );
    state["phase"] = json!("needs_attention");
    state["error"] = json!("model changed");
    util::atomic_write_json(&worker.join("state.json"), &state).unwrap();
    let s = h.tick(None);
    let event = model::get(&s, "events")
        .and_then(Value::as_object)
        .and_then(|events| events.values().next())
        .cloned()
        .expect("event");
    assert_eq!(
        model::string(&event, "outcome").as_deref(),
        Some("decision")
    );
    assert!(model::string(&event, "summary")
        .unwrap_or_default()
        .contains("model changed"));
}

#[test]
fn ci_wait_identity_is_pinned_and_missing_source_is_an_error() {
    let h = Harness::new();
    let source = h.base.join("ci");
    common::create_dir_all(&source).unwrap();
    let config = json!({"task": "o/r#1", "epoch": 1, "sha": SHA});
    let hash = util::sha256_json_sorted(&config);
    util::atomic_write_json(
        &source.join("identity.json"),
        &json!({"config_hash": hash, "config": config}),
    )
    .unwrap();
    util::atomic_write_json(
        &source.join("status.json"),
        &json!({"config_hash": hash, "state": "success"}),
    )
    .unwrap();
    h.enable();
    h.add(
        "build",
        None,
        json!({
            "kind": "ci",
            "source": source.display().to_string(),
            "config_hash": hash
        }),
    );
    let s = h.tick(Some(10.0));
    let event_id = event_for(&s, "build");
    assert_eq!(
        model::dig_str(&s, &["events", &event_id, "outcome"]).as_deref(),
        Some("complete")
    );
}

#[test]
fn queue_summary_contains_diagnosis() {
    let h = Harness::new();
    h.enable();
    h.add("step1", Some("assignment1"), json!({}));
    let mut s = queue::load_queue(&h.queue).unwrap();
    herdr_flow::progress::register(&mut s, &h.task_spec(), 100.0).unwrap();
    queue::save(&h.queue, &mut s).unwrap();
    let text = queue::summary(&queue::load_queue(&h.queue).unwrap());
    assert!(text.contains("diagnosis="));
    assert!(text.contains("notification="));
    assert!(text.contains("next="));
}

#[test]
fn enable_and_readiness_require_the_exact_live_receiver() {
    let h = Harness::new();
    h.agent.lock().unwrap().session = Some("unrelated".to_string());
    let factory = factory(h.agent.clone());
    let mut s = queue::load_queue(&h.queue).unwrap();
    assert!(queue::enable(&h.queue, &mut s, &factory).is_err());
    assert!(!model::bool_or(
        &queue::load_queue(&h.queue).unwrap(),
        "notify",
        true
    ));
    h.agent.lock().unwrap().session = Some(SESSION.to_string());
    let mut s = queue::load_queue(&h.queue).unwrap();
    queue::enable(&h.queue, &mut s, &factory).unwrap();
    let readiness = herdr_flow::runtime::readiness(&h.queue, Some(SESSION), &factory).unwrap();
    assert!(model::bool_or(&readiness, "active_wakeup", false));
    assert!(model::bool_or(&readiness, "context_telemetry", false));
    assert!(!model::bool_or(&readiness, "safe_rotation", true));
    let wrong = herdr_flow::runtime::readiness(&h.queue, Some("other"), &factory).unwrap();
    assert!(!model::bool_or(&wrong, "active_wakeup", true));
    assert!(!model::array_field(&wrong, "gaps").unwrap().is_empty());
    let mut guard_state = util::read_json(&h.guard.join("state.json")).unwrap();
    guard_state["auto_rotate"] = json!(true);
    guard_state["usage"] = Value::Null;
    guard_state["level"] = json!("unknown");
    util::atomic_write_json(&h.guard.join("state.json"), &guard_state).unwrap();
    let readiness = herdr_flow::runtime::readiness(&h.queue, Some(SESSION), &factory).unwrap();
    assert!(model::bool_or(&readiness, "active_wakeup", false));
    assert!(!model::bool_or(&readiness, "safe_rotation", true));
}

#[test]
fn dispatch_application_requires_current_notification_readiness() {
    let h = Harness::new();
    h.enable();
    h.add("step1", Some("assignment1"), json!({}));
    let mut s = queue::load_queue(&h.queue).unwrap();
    herdr_flow::progress::register(&mut s, &h.task_spec(), 100.0).unwrap();
    queue::save(&h.queue, &mut s).unwrap();
    queue::signal(
        &h.queue,
        "step1",
        "complete",
        "result published",
        Some("/evidence"),
    )
    .unwrap();
    let s = h.tick(Some(110.0));
    let event_id = event_for(&s, "step1");
    let active = model::string(&s, "active").unwrap();
    let mut s = queue::acknowledge(&h.queue, &active, SESSION).unwrap();
    let mut queue_state = queue::load_queue(&h.queue).unwrap();
    queue_state["notify"] = json!(false);
    queue::save(&h.queue, &mut queue_state).unwrap();
    let version = model::int(
        model::dig(&s, &["tasks", "o/r#1"]).unwrap(),
        "state_version",
    )
    .unwrap();
    let spec = json!({
        "task": "o/r#1",
        "state_version": version,
        "event": event_id,
        "acceptance": {"first": "/first-result"},
        "next_action": {
            "kind": "dispatch",
            "operation": "op2",
            "assignment": "assignment2",
            "session": "22222222-2222-2222-2222-222222222222",
            "workspace": h.base.join("work").display().to_string(),
            "owner": "dev",
            "instruction": "continue",
            "action": "development",
            "authorization": "develop",
            "wait": "step2",
            "accept_timeout_seconds": 20
        }
    });
    let factory = factory(h.agent.clone());
    let error =
        queue::task_action(&h.queue, "task-apply", &spec, Some(SESSION), &factory).unwrap_err();
    assert_eq!(error.kind, herdr_flow::ErrorKind::Queue);
    assert!(error.message.contains("readiness"));
    let _ = &mut s;
}

#[test]
fn add_rejects_non_object_waits_instead_of_panicking() {
    let h = Harness::new();
    let mut state = util::read_json(&h.queue.join("queue.json")).unwrap();
    state["waits"] = json!([]);
    util::atomic_write_json(&h.queue.join("queue.json"), &state).unwrap();
    let result = queue::add(
        &h.queue,
        &json!({
            "id": "w1",
            "task": "o/r#1",
            "epoch": 1,
            "owner": "dev",
            "reason": "x",
            "condition": "y",
            "kind": "signal",
            "timeout_seconds": 10
        }),
    );
    assert!(result.is_err(), "{result:?}");
}

#[test]
fn lineage_actions_and_result_ack_persist_in_the_existing_queue() {
    let h = Harness::new();
    h.enable();
    let factory = factory(h.agent.clone());
    let run =
        |spec: &J| queue::task_action(&h.queue, "task-lineage", spec, Some(SESSION), &factory);
    let plan = json!({"kind":"plan","task":"document", "goal":"deliver document",
        "basis":{"original":["user request"],"changes":[],"specifications":[],"endpoint":"customer location","confirmation":"user request"},
        "authorization":[{"id":"develop","action":"development","source":"user request"}]});
    let s = run(&plan).unwrap();
    let path = h.base.join("document");
    std::fs::write(&path, "document").unwrap();
    let id = util::pyjson::sha256_hex(b"document");
    let s=run(&json!({"kind":"artifact","task":"document","state_version":s["tasks"]["document"]["state_version"],
        "artifact":{"id":id,"adapter":"file-sha256","location":path}})).unwrap();
    let s=run(&json!({"kind":"candidate","task":"document","state_version":s["tasks"]["document"]["state_version"],"candidate":id,"reason":"ready"})).unwrap();
    run(&json!({"kind":"activity","task":"document","state_version":s["tasks"]["document"]["state_version"],
        "activity":"review-document","role":"acceptance","input":id,"session_epoch":1,
        "result_wait":{"id":"review-document","epoch":1,"assignment":"review-a1","owner":"review","kind":"signal","reason":"review","condition":"result","timeout_seconds":600}})).unwrap();
    let mut registered = queue::load_queue(&h.queue).unwrap();
    registered["waits"]["review-document"]["producer"] =
        json!({"adapter":"codex","session":SESSION});
    let verdict = herdr_flow::progress::acceptance::submit(&mut registered,"review-document",&json!({"adapter":"codex","session":SESSION}),&json!({"conclusion":"passed","scope":"document","checks":[{"id":"content","criterion":"goal","source":"user request","status":"passed","evidence":"independently checked evidence"}],"gaps":[],"reuses":[],"resolutions":[]})).unwrap();
    queue::save(&h.queue, &mut registered).unwrap();
    let signal = std::process::Command::new(env!("CARGO_BIN_EXE_herdr-flow"))
        .args([
            "queue",
            "signal",
            "--state-dir",
            h.queue.to_str().unwrap(),
            "--wait",
            "review-document",
            "--outcome",
            "complete",
            "--summary",
            "verified",
        ])
        .env("CODEX_THREAD_ID", SESSION)
        .output()
        .unwrap();
    assert!(
        signal.status.success(),
        "{}",
        String::from_utf8_lossy(&signal.stderr)
    );
    let s = h.tick(None);
    let e = event_for(&s, "review-document");
    let active = s["active"].as_str().unwrap();
    assert!(!s["events"][&e]["acknowledged"].as_bool().unwrap());
    let s = queue::acknowledge(&h.queue, active, SESSION).unwrap();
    assert!(
        s["tasks"]["document"]["approved"].is_null(),
        "ACK is receipt, not approval"
    );
    let apply = json!({"task":"review-document","state_version":s["tasks"]["review-document"]["state_version"],"event":e,
        "verdict":verdict["id"],"next_action":{"kind":"delivery","owner":"coordinator","reason":"independent pass","condition":"existing delivery authorization"}});
    let s = queue::task_action(&h.queue, "task-apply", &apply, Some(SESSION), &factory).unwrap();
    assert_eq!(s["tasks"]["document"]["approved"]["candidate"], id);
    assert_eq!(queue::load_queue(&h.queue).unwrap(), s);
    assert!(queue::task_action(&h.queue, "task-apply", &apply, Some(SESSION), &factory).is_err());
    assert_eq!(queue::load_queue(&h.queue).unwrap(), s);
}
