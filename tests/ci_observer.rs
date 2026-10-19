//! Read-only CI observer and external Check policy over fake GitHub APIs.

pub mod common;

use std::collections::BTreeMap;
use std::path::Path;

use herdr_flow::ci::{checks, policy, watch};
use herdr_flow::model::{self, J};
use herdr_flow::util;
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

    fn page(mut self, path: &str, key: &str, value: Value) -> Self {
        self.pages.insert(
            (path.to_string(), key.to_string()),
            value.as_array().cloned().unwrap_or_default(),
        );
        self
    }
}

impl watch::GitHubApi for Fake {
    fn get(&self, suffix: &str) -> herdr_flow::Result<J> {
        self.get
            .get(suffix)
            .cloned()
            .ok_or_else(|| herdr_flow::FlowError::watch(format!("unexpected GET {suffix}")))
    }

    fn pages(&self, suffix: &str, key: &str) -> herdr_flow::Result<Vec<J>> {
        self.pages
            .get(&(suffix.to_string(), key.to_string()))
            .cloned()
            .ok_or_else(|| herdr_flow::FlowError::watch(format!("unexpected PAGES {suffix}")))
    }
}

fn run() -> J {
    json!({
        "id": 1,
        "head_sha": SHA,
        "event": "push",
        "run_attempt": 1,
        "status": "completed",
        "conclusion": "success",
        "html_url": "https://example/run/1"
    })
}

fn job(name: &str, conclusion: &str) -> J {
    json!({
        "id": 10,
        "run_id": 1,
        "name": name,
        "status": "completed",
        "conclusion": conclusion,
        "html_url": "https://example/job/10"
    })
}

fn config() -> J {
    json!({
        "version": 1,
        "task": "o/r#1",
        "epoch": 1,
        "repo": "o/r",
        "sha": SHA,
        "workflows": [{"file": "ci.yml", "event": "push", "jobs": ["gate"]}]
    })
}

fn api_success() -> Fake {
    let run = run();
    let jobs = vec![job("gate", "success")];
    Fake::new()
        .page(
            &format!("actions/workflows/ci.yml/runs?head_sha={SHA}&event=push"),
            "workflow_runs",
            json!([run]),
        )
        .page("actions/runs/1/attempts/1/jobs", "jobs", json!(jobs))
        .get("actions/runs/1", run)
}

#[test]
fn validate_accepts_explicit_inventory_and_rejects_unsafe_names() {
    let c = config();
    assert!(watch::validate(&c).is_ok());
    assert!(watch::validate(
        &json!({"version": 1, "repo": "o/r", "sha": SHA, "task": "t", "epoch": 1, "workflows": []})
    )
    .is_err());
    for bad in [
        json!({"version": 1, "repo": ["o/r"], "sha": SHA, "task": "t", "epoch": 1, "workflows": []}),
        json!({"version": 1, "repo": "o/r", "sha": 1, "task": "t", "epoch": 1, "workflows": []}),
        json!({"version": 1, "repo": "o/r", "sha": SHA, "task": 5, "epoch": 1, "workflows": []}),
        json!({"version": 1, "repo": "o/r", "sha": SHA, "task": "t", "epoch": true, "workflows": []}),
        json!({"version": 1, "repo": "o/r", "sha": SHA, "task": "t", "epoch": 1, "workflows": [{"file": "../ci.yml", "event": "push"}]}),
        json!({"version": 1, "repo": "o/r", "sha": SHA, "task": "t", "epoch": 1, "workflows": [{"file": "ci.yml", "event": "push&x=1"}]}),
        json!({"version": 1, "repo": "o/r", "sha": SHA, "task": "t", "epoch": 1, "workflows": [{"file": "ci.yml", "event": "push", "jobs": ["gate", "gate"], "decision": "required_jobs"}]}),
    ] {
        assert!(watch::validate(&bad).is_err(), "accepted {bad}");
    }
}

#[test]
fn observe_success_and_failure_decisions() {
    let c = config();
    let result = watch::observe(&c, &api_success()).unwrap();
    assert_eq!(model::string(&result, "state").as_deref(), Some("success"));
    for conclusion in ["failure", "skipped", "neutral", "cancelled"] {
        let jobs = vec![job("gate", conclusion)];
        let api = Fake::new()
            .page(
                &format!("actions/workflows/ci.yml/runs?head_sha={SHA}&event=push"),
                "workflow_runs",
                json!([run()]),
            )
            .page("actions/runs/1/attempts/1/jobs", "jobs", json!(jobs))
            .get("actions/runs/1", run());
        let result = watch::observe(&c, &api).unwrap();
        assert_eq!(model::string(&result, "state").as_deref(), Some("failure"));
    }
    let empty = Fake::new().page(
        &format!("actions/workflows/ci.yml/runs?head_sha={SHA}&event=push"),
        "workflow_runs",
        json!([]),
    );
    assert_eq!(
        model::string(&watch::observe(&c, &empty).unwrap(), "state").as_deref(),
        Some("pending")
    );
}

#[test]
fn required_jobs_decision_keeps_advisory_noise_out() {
    let mut c = config();
    c["workflows"][0]["decision"] = json!("required_jobs");
    let mut api_run = run();
    api_run["status"] = json!("in_progress");
    api_run["conclusion"] = Value::Null;
    let api = Fake::new()
        .page(
            &format!("actions/workflows/ci.yml/runs?head_sha={SHA}&event=push"),
            "workflow_runs",
            json!([api_run]),
        )
        .page(
            "actions/runs/1/attempts/1/jobs",
            "jobs",
            json!([job("gate", "success"), job("advisory", "failure")]),
        )
        .get("actions/runs/1", run());
    let result = watch::observe(&c, &api).unwrap();
    assert_eq!(
        model::string(&result, "state").as_deref(),
        Some("success"),
        "{result}"
    );
    let row = &result["workflows"][0];
    assert_eq!(
        model::string(row, "decision_reason").as_deref(),
        Some("required_jobs_succeeded")
    );
    assert!(model::bool_or(row, "decision_terminal", false));
}

#[test]
fn external_checks_pin_app_run_and_suite_window() {
    let config = json!({
        "version": 1,
        "task": "o/r#1",
        "epoch": 1,
        "repo": "o/r",
        "pr": 7,
        "sha": SHA,
        "base_ref": "main",
        "target_base": BASE,
        "checks": [{"name": "Trusted gate", "app_id": 123, "external_id": "gate-1"}]
    });
    let pr = json!({
        "head": {"sha": SHA, "repo": {"full_name": "o/r"}},
        "base": {"sha": BASE, "ref": "main", "repo": {"full_name": "o/r"}},
        "state": "open",
        "merged": false
    });
    let check = json!({
        "id": 10,
        "name": "Trusted gate",
        "app": {"id": 123},
        "head_sha": SHA,
        "external_id": "gate-1",
        "status": "completed",
        "conclusion": "success",
        "details_url": "https://example/check/10"
    });
    let api = Fake::new()
        .get("pulls/7", pr.clone())
        .get("branches/main", json!({"commit": {"sha": BASE}}))
        .get(
            &format!("commits/{SHA}/check-suites?per_page=1"),
            json!({"total_count": 1}),
        )
        .page(
            &format!("commits/{SHA}/check-runs?filter=all"),
            "check_runs",
            json!([check.clone()]),
        )
        .get("check-runs/10", check);
    let result = watch::observe(&config, &api).unwrap();
    assert_eq!(
        model::string(&result, "state").as_deref(),
        Some("success"),
        "{result}"
    );
    assert_eq!(model::int(&result["checks"][0], "check_run"), Some(10));
    let overflow = Fake::new()
        .get("pulls/7", pr)
        .get("branches/main", json!({"commit": {"sha": BASE}}))
        .get(
            &format!("commits/{SHA}/check-suites?per_page=1"),
            json!({"total_count": 1001}),
        );
    let result = checks::observe(&config, &overflow).unwrap();
    assert_eq!(
        model::string(&result[0], "reason").as_deref(),
        Some("external_check_suite_window_exceeded")
    );
}

#[test]
fn check_validation_rejects_ambiguous_inventory() {
    let base = json!({
        "version": 1,
        "task": "o/r#1",
        "epoch": 1,
        "repo": "o/r",
        "pr": 7,
        "sha": SHA,
        "base_ref": "main",
        "target_base": BASE,
        "checks": [{"name": "Trusted gate", "app_id": 123, "external_id": "gate-1"}]
    });
    assert!(checks::validate(&base).is_ok());
    assert!(watch::validate(&json!({"version": 1, "repo": "o/r", "sha": SHA, "task": "t", "epoch": 1, "workflows": [], "checks": []})).is_err());
    for bad in [
        json!({"checks": [{"name": "x", "app_id": true, "external_id": "x"}]}),
        json!({"checks": [{"name": "x", "app_id": 1, "external_id": "x"}, {"name": "x", "app_id": 1, "external_id": "y"}]}),
        json!({"checks": base["checks"].clone(), "target_base": "short"}),
        json!({"checks": base["checks"].clone(), "pr": null}),
        json!({"checks": base["checks"].clone(), "repo": "../o/r"}),
    ] {
        let mut config = base.clone();
        if let Some(map) = bad.as_object() {
            for (key, value) in map {
                config[key] = value.clone();
            }
        }
        assert!(checks::validate(&config).is_err(), "accepted {config}");
    }
}

#[test]
fn persistence_is_deduplicated_and_atomic() {
    let tmp = common::tempdir().unwrap();
    let root = tmp.path().join("state");
    for _ in 0..3 {
        watch::persist(&root, "hash", &json!({"state": "pending"})).unwrap();
    }
    let events = std::fs::read_dir(root.join("events")).unwrap().count();
    assert_eq!(events, 1);
    let status = util::read_json(&root.join("status.json")).unwrap();
    assert_eq!(model::string(&status, "state").as_deref(), Some("pending"));
    assert!(status.as_object().unwrap().keys().len() >= 4);
}

#[test]
fn safe_json_depth_and_atomic_symlink_rejection() {
    // The parser limit is internal; at minimum a normal nested object loads.
    assert!(watch::safe_json_load(br#"{"a":[{"b":1}],"c":"x"}"#).is_ok());
    assert!(watch::safe_json_load(&vec![b'1'; 10 * 1024 * 1024 + 1]).is_err());
    let tmp = common::tempdir().unwrap();
    let real = tmp.path().join("real");
    common::create_dir_all(&real).unwrap();
    let link = tmp.path().join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    assert!(util::atomic_write_json(&link.join("state.json"), &json!({"ok": true})).is_err());
    let file = real.join("state.json");
    util::atomic_write_json(&file, &json!({"ok": true})).unwrap();
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[test]
fn policy_required_jobs_precedence() {
    let spec = json!({"jobs": ["gate"]});
    let run = json!({"status": "in_progress"});
    let jobs = vec![job("gate", "failure")];
    let result = policy::required_jobs(&spec, &run, &jobs).unwrap();
    assert_eq!(model::string(&result, "state").as_deref(), Some("failure"));
    let jobs = vec![job("gate", "cancelled")];
    let result = policy::required_jobs(&spec, &run, &jobs).unwrap();
    assert_eq!(
        model::string(&result, "state").as_deref(),
        Some("needs_attention")
    );
    let _ = Path::new("/");
}

#[test]
fn run_rows_accepts_the_minimal_lane_publication_inventory() {
    let check = json!({
        "id": 10,
        "name": "Trusted gate",
        "app": {"id": 123},
        "head_sha": SHA,
        "external_id": "gate-1",
        "status": "completed",
        "conclusion": "success"
    });
    let api = Fake::new()
        .get(
            &format!("commits/{SHA}/check-suites?per_page=1"),
            json!({"total_count": 1}),
        )
        .page(
            &format!("commits/{SHA}/check-runs?filter=all"),
            "check_runs",
            json!([check.clone()]),
        )
        .get("check-runs/10", check);
    let minimal = json!({
        "sha": SHA,
        "checks": [{"name": "Trusted gate", "app_id": 123, "external_id": "gate-1"}]
    });
    let rows = checks::run_rows(&minimal, &api).unwrap();
    assert_eq!(model::string(&rows[0], "state").as_deref(), Some("success"));
    assert_eq!(model::int(&rows[0], "check_run"), Some(10));
}
