//! Focused offline checks for the group-D refactor.
//!
//! These tests exercise the refactored PR observer and repair decision paths
//! without any network access and without modifying durable queue documents.

pub mod common;

use std::collections::BTreeMap;
use std::path::Path;

use herdr_flow::ci::{self, GitHubApi};
use herdr_flow::model::{self, J};
use herdr_flow::pr;
use herdr_flow::util::pyjson::sha256_json_sorted;
use herdr_flow::{ErrorKind, FlowError, Result};
use serde_json::{json, Value};

const SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const BASE: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

struct Fake {
    get: BTreeMap<String, J>,
    pages: BTreeMap<(String, String), Vec<J>>,
}

impl Fake {
    fn new() -> Self {
        Self {
            get: BTreeMap::new(),
            pages: BTreeMap::new(),
        }
    }

    fn get(mut self, path: &str, value: Value) -> Self {
        self.get.insert(path.to_string(), value);
        self
    }
}

impl GitHubApi for Fake {
    fn get(&self, suffix: &str) -> Result<J> {
        self.get
            .get(suffix)
            .cloned()
            .ok_or_else(|| FlowError::watch(format!("unexpected GET {suffix}")))
    }

    fn pages(&self, suffix: &str, key: &str) -> Result<Vec<J>> {
        self.pages
            .get(&(suffix.to_string(), key.to_string()))
            .cloned()
            .ok_or_else(|| FlowError::watch(format!("unexpected PAGES {suffix}")))
    }
}

fn pull() -> J {
    json!({
        "number": 7,
        "head": {"sha": SHA, "ref": "feature", "repo": {"full_name": "o/r"}},
        "base": {"sha": BASE, "ref": "main", "repo": {"full_name": "o/r"}},
        "state": "open",
        "merged": false,
        "mergeable": true,
        "auto_merge": {"enabled": true}
    })
}

fn task() -> J {
    json!({
        "task": "o/r#1",
        "epoch": 1,
        "assignment": "a1",
        "candidate": SHA,
        "goal": "ship",
        "owner": "dev",
        "state_version": 1,
        "phase": "blocked",
        "control": "running",
        "wait": "step1",
        "next_action": {"kind": "blocked", "owner": "dev", "reason": "done", "condition": "publish"},
        "acceptance": {},
        "authorization": {},
        "pending": [],
        "operations": {},
        "findings": {},
        "history": [],
        "created_at": 1.0,
        "apply_timeout": 600,
        "writer": null,
        "pr": {
            "number": 7,
            "repo": "o/r",
            "host": "github.com",
            "checkout": "/tmp/refactor-d/checkout",
            "original_base": BASE,
            "head": SHA,
            "own_commits": [SHA],
            "base_ref": "main",
            "head_ref": "feature",
            "target_base": BASE,
            "issue": 7,
            "max_repairs": 2,
            "repairs": {},
            "versions": [],
            "dependencies": [],
            "phase": "registered",
            "publication": {"kind": "create", "phase": "observed"},
            "landing": null,
            "observation": null
        }
    })
}

fn queue_state(task: J) -> J {
    json!({
        "version": 1,
        "id": "q1",
        "guard_dir": null,
        "receiver": null,
        "poll_seconds": 30,
        "notify": false,
        "stopped": false,
        "waits": {},
        "events": {},
        "deliveries": {},
        "active": null,
        "delivery_status": "disabled",
        "tasks": {"o/r#1": task}
    })
}

fn observer_config() -> J {
    json!({
        "version": 1,
        "task": "o/r#1",
        "epoch": 1,
        "repo": "o/r",
        "host": "github.com",
        "pr": 7,
        "sha": SHA,
        "workflows": [{
            "file": "ci.yml",
            "event": "push",
            "jobs": ["gate"],
            "decision": "required_jobs"
        }]
    })
}

fn write_observer(dir: &Path, state: &str, conclusion: &str) -> f64 {
    let config = ci::validate(&observer_config()).expect("valid observer config");
    let digest = sha256_json_sorted(&config);
    let now = 100.0;
    let identity = json!({"config": config, "config_hash": digest});
    std::fs::write(
        dir.join("identity.json"),
        serde_json::to_vec(&identity).unwrap(),
    )
    .unwrap();
    let status = json!({
        "config_hash": digest,
        "observed_at": now,
        "state": state,
        "workflows": [{
            "file": "ci.yml",
            "event": "push",
            "run_id": null,
            "attempt": 1,
            "jobs": [{
                "id": 10,
                "name": "gate",
                "status": "completed",
                "conclusion": conclusion
            }]
        }]
    });
    std::fs::write(
        dir.join("status.json"),
        serde_json::to_vec(&status).unwrap(),
    )
    .unwrap();
    now
}

#[test]
fn error_kind_type_names_match_legacy_names() {
    let pairs = [
        (ErrorKind::Queue, "QueueError"),
        (ErrorKind::Progress, "ProgressError"),
        (ErrorKind::Guard, "GuardError"),
        (ErrorKind::Transient, "TransientError"),
        (ErrorKind::RpcProtocol, "RpcProtocolError"),
        (ErrorKind::Timeout, "TimeoutExpired"),
        (ErrorKind::Io, "OSError"),
        (ErrorKind::Json, "ValueError"),
        (ErrorKind::Other, "Error"),
    ];
    for (kind, name) in pairs {
        assert_eq!(kind.type_name(), name);
    }
}

#[test]
fn dig_mut_walks_nested_objects_and_misses() {
    let mut state = json!({"tasks": {"t": {"pr": {"phase": "registered"}}}});
    *model::dig_mut(&mut state, &["tasks", "t", "pr", "phase"]).unwrap() = json!("observed");
    assert_eq!(
        model::dig_str(&state, &["tasks", "t", "pr", "phase"]).as_deref(),
        Some("observed")
    );
    assert!(model::dig_mut(&mut state, &["tasks", "missing", "pr"]).is_none());
}

#[test]
fn sorted_value_copies_nested_documents() {
    let value = json!({"z": [{"b": 1, "a": 2}], "a": {"d": 4, "c": 3}});
    let sorted = model::sorted_value(&value);
    assert_eq!(sorted["a"]["c"], json!(3));
    assert_eq!(sorted["z"][0]["a"], json!(2));
    assert_eq!(model::sorted_value(&json!([1, 2, 3])), json!([1, 2, 3]));
}

#[test]
fn unknown_pr_action_is_rejected() {
    let mut state = json!({});
    assert!(pr::action(&mut state, "pr-unknown", &json!({}), 0.0).is_err());
}

#[test]
fn observe_records_required_workflow_failure() {
    let dir = common::tempdir().unwrap();
    let observed_at = write_observer(dir.path(), "failure", "failure");
    let api = Fake::new().get("pulls/7", pull());
    let mut state = queue_state(task());
    let spec = json!({
        "task": "o/r#1",
        "state_version": 1,
        "source": dir.path().display().to_string()
    });
    let updated = pr::observe(&mut state, &spec, observed_at + 10.0, Some(&api)).unwrap();
    assert_eq!(
        model::dig_str(&updated, &["pr", "phase"]).as_deref(),
        Some("repair_needed")
    );
    let observation = model::dig(&updated, &["pr", "observation"]).unwrap();
    assert_eq!(
        model::string(observation, "state").as_deref(),
        Some("failure")
    );
    assert!(model::string(observation, "token").is_some());
    let failures = model::array_field(observation, "failures").unwrap();
    assert_eq!(failures.len(), 1);
    assert_eq!(model::string(&failures[0], "name").as_deref(), Some("gate"));
    assert_eq!(model::int(&updated, "state_version"), Some(2));
    assert_eq!(
        model::dig_int(&state, &["tasks", "o/r#1", "state_version"]),
        Some(2)
    );
}

#[test]
fn observe_records_successful_required_workflow() {
    let dir = common::tempdir().unwrap();
    let observed_at = write_observer(dir.path(), "success", "success");
    let api = Fake::new().get("pulls/7", pull());
    let mut state = queue_state(task());
    let spec = json!({
        "task": "o/r#1",
        "state_version": 1,
        "source": dir.path().display().to_string()
    });
    let updated = pr::observe(&mut state, &spec, observed_at + 10.0, Some(&api)).unwrap();
    assert_eq!(
        model::dig_str(&updated, &["pr", "phase"]).as_deref(),
        Some("checks_satisfied")
    );
    let observation = model::dig(&updated, &["pr", "observation"]).unwrap();
    assert_eq!(
        model::array_field(observation, "failures")
            .map(Vec::len)
            .unwrap_or_default(),
        0
    );
}

#[test]
fn repair_intent_records_isolated_dispatch() {
    let mut task = task();
    task["pr"]["phase"] = json!("repair_needed");
    task["pr"]["observation"] = json!({
        "token": "tok",
        "at": 100.0,
        "source": "/tmp/refactor-d/ci/status.json",
        "failures": []
    });
    let action = json!({
        "operation": "op2",
        "writes": true,
        "workspace": "/tmp/refactor-d/repair-work",
        "pr_repair": {"number": 7, "head": SHA, "base": BASE, "token": "tok"},
        "repair_diagnosis": {"kind": "code", "evidence": "/tmp/refactor-d/diagnosis"}
    });
    let event = json!({
        "outcome": "failed",
        "evidence": "/tmp/refactor-d/ci/status.json"
    });
    pr::repair_intent(&mut task, &action, &event, 110.0).unwrap();
    let used = model::dig(&task, &["pr", "repairs", "tok"]).unwrap();
    assert_eq!(model::string(used, "operation").as_deref(), Some("op2"));
    assert_eq!(model::string(used, "head").as_deref(), Some(SHA));
    assert_eq!(model::string(used, "base").as_deref(), Some(BASE));
    assert_eq!(model::number(used, "at"), Some(110.0));
}

#[test]
fn check_repair_accepts_fresh_observation_offline() {
    let mut task = task();
    task["pr"]["phase"] = json!("repair_needed");
    task["pr"]["observation"] = json!({
        "token": "tok",
        "at": 100.0,
        "source": "/tmp/refactor-d/ci/status.json",
        "failures": []
    });
    let op = json!({
        "intent": {
            "writes": true,
            "pr_repair": {"number": 7, "head": SHA, "base": BASE, "token": "tok"}
        }
    });
    let state = json!({});
    pr::check_repair(&state, &task, &op, 110.0, false).unwrap();
}
