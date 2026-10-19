//! Focused offline tests for the refactored CI observation, metrics and
//! watcher-service helpers. Everything here is pure Rust and network-free.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::time::Duration;

use herdr_flow::ci::watch::GitHubApi;
use herdr_flow::ci::{checks, watch};
use herdr_flow::metrics;
use herdr_flow::model;
use herdr_flow::util;
use herdr_flow::watch_service;
use serde_json::{json, Value};
use tempfile::TempDir;

const SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

// ---------------------------------------------------------------------------
// GitHubApi pagination default
// ---------------------------------------------------------------------------

struct PageApi {
    chunks: Vec<Vec<Value>>,
    calls: RefCell<Vec<String>>,
}

impl PageApi {
    fn new(chunks: Vec<Vec<Value>>) -> Self {
        Self {
            chunks,
            calls: RefCell::new(Vec::new()),
        }
    }
}

impl watch::GitHubApi for PageApi {
    fn get(&self, suffix: &str) -> herdr_flow::Result<Value> {
        self.calls.borrow_mut().push(suffix.to_string());
        let page = suffix
            .rsplit_once("page=")
            .and_then(|(_, page)| page.parse::<usize>().ok())
            .unwrap_or(0);
        let chunk = page
            .checked_sub(1)
            .and_then(|index| self.chunks.get(index))
            .cloned()
            .unwrap_or_default();
        Ok(json!({"items": chunk}))
    }
}

struct FixedApi {
    payload: Value,
}

impl watch::GitHubApi for FixedApi {
    fn get(&self, _suffix: &str) -> herdr_flow::Result<Value> {
        Ok(self.payload.clone())
    }
}

fn numbered(count: usize) -> Vec<Value> {
    (0..count).map(|index| json!({"n": index})).collect()
}

#[test]
fn pagination_default_walks_until_a_short_page() {
    let api = PageApi::new(vec![numbered(100), numbered(3)]);
    let items = api.pages("actions/runs?head=1", "items").unwrap();
    assert_eq!(items.len(), 103);
    let calls = api.calls.borrow();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0], "actions/runs?head=1&per_page=100&page=1");
    assert_eq!(calls[1], "actions/runs?head=1&per_page=100&page=2");
}

#[test]
fn pagination_default_adds_a_question_mark_and_honours_the_limit() {
    let api = PageApi::new(vec![numbered(2)]);
    let items = api.pages("actions/check-runs", "items").unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(
        api.calls.borrow()[0],
        "actions/check-runs?per_page=100&page=1"
    );

    let full = (0..100).map(|_| numbered(100)).collect();
    let api = PageApi::new(full);
    assert!(api.pages("actions/check-runs", "items").is_err());
    assert_eq!(api.calls.borrow().len(), 100);
}

#[test]
fn pagination_default_rejects_malformed_chunks() {
    let missing = FixedApi {
        payload: json!({"other": []}),
    };
    assert!(missing.pages("x", "items").is_err());
    let not_array = FixedApi {
        payload: json!({"items": 3}),
    };
    assert!(not_array.pages("x", "items").is_err());
    let empty = FixedApi {
        payload: json!({"items": []}),
    };
    assert!(empty.pages("x", "items").unwrap().is_empty());
}

// ---------------------------------------------------------------------------
// CLI parsing
// ---------------------------------------------------------------------------

#[test]
fn cli_parses_offline_arguments_without_spawning() {
    assert_eq!(watch::cli(&["-h".to_string()]), 0);
    assert_eq!(watch::cli(&["--help".to_string()]), 0);
    assert_eq!(watch::cli(&["--bogus".to_string()]), 2);
    assert_eq!(watch::cli(&["--config".to_string()]), 2);
    assert_eq!(watch::cli(&["--once".to_string()]), 2);
    assert_eq!(
        watch::cli(&[
            "--config".to_string(),
            "/tmp/config.json".to_string(),
            "--state-dir".to_string(),
            "/tmp/state".to_string(),
            "--ready-fd".to_string(),
            "not-a-fd".to_string(),
        ]),
        2
    );
    assert_eq!(
        watch::cli(&[
            "--config".to_string(),
            "/tmp/config.json".to_string(),
            "--state-dir".to_string(),
            "/tmp/state".to_string(),
            "--background".to_string(),
            "--once".to_string(),
        ]),
        2
    );
}

// ---------------------------------------------------------------------------
// Validation edges
// ---------------------------------------------------------------------------

fn base_config() -> Value {
    json!({
        "version": 1,
        "task": "o/r#1",
        "epoch": 1,
        "repo": "o/r",
        "sha": SHA
    })
}

#[test]
fn workflow_validation_rejects_ambiguous_inventory() {
    let mut valid = base_config();
    valid["workflows"] = json!([{"file": "ci.yml", "event": "push"}]);
    assert!(watch::validate(&valid).is_ok());

    let mut duplicate = valid.clone();
    duplicate["workflows"] = json!([
        {"file": "ci.yml", "event": "push"},
        {"file": "ci.yml", "event": "push"}
    ]);
    assert!(watch::validate(&duplicate).is_err());

    let mut non_string_jobs = valid.clone();
    non_string_jobs["workflows"] =
        json!([{"file": "ci.yml", "event": "push", "jobs": ["gate", 3]}]);
    assert!(watch::validate(&non_string_jobs).is_err());

    let mut bad_decision = valid.clone();
    bad_decision["workflows"] =
        json!([{"file": "ci.yml", "event": "push", "decision": "anything"}]);
    assert!(watch::validate(&bad_decision).is_err());

    let mut required_jobs = valid.clone();
    required_jobs["workflows"] = json!([{
        "file": "ci.yml",
        "event": "push",
        "decision": "required_jobs",
        "jobs": ["gate", "test"]
    }]);
    assert!(watch::validate(&required_jobs).is_ok());

    for jobs in [
        json!([]),
        json!(["gate", "gate"]),
        json!([" gate"]),
        json!([" "]),
    ] {
        let mut bad = required_jobs.clone();
        bad["workflows"][0]["jobs"] = jobs;
        assert!(watch::validate(&bad).is_err(), "accepted {bad}");
    }

    let mut not_array = valid.clone();
    not_array["workflows"] = json!({"file": "ci.yml"});
    assert!(watch::validate(&not_array).is_err());
}

#[test]
fn identity_and_range_validation_rejects_bad_values() {
    let mut empty = base_config();
    empty["workflows"] = json!([]);
    assert!(watch::validate(&empty).is_err());

    let mut bad_version = base_config();
    bad_version["version"] = json!(2);
    bad_version["workflows"] = json!([{"file": "ci.yml", "event": "push"}]);
    assert!(watch::validate(&bad_version).is_err());

    let mut bad_repo = base_config();
    bad_repo["repo"] = json!("../o/r");
    bad_repo["workflows"] = json!([{"file": "ci.yml", "event": "push"}]);
    assert!(watch::validate(&bad_repo).is_err());

    let mut bad_sha = base_config();
    bad_sha["sha"] = json!("short");
    bad_sha["workflows"] = json!([{"file": "ci.yml", "event": "push"}]);
    assert!(watch::validate(&bad_sha).is_err());

    let mut bad_host = base_config();
    bad_host["host"] = json!("bad host");
    bad_host["workflows"] = json!([{"file": "ci.yml", "event": "push"}]);
    assert!(watch::validate(&bad_host).is_err());

    let mut bad_epoch = base_config();
    bad_epoch["epoch"] = json!(true);
    bad_epoch["workflows"] = json!([{"file": "ci.yml", "event": "push"}]);
    assert!(watch::validate(&bad_epoch).is_err());

    for (key, value) in [
        ("interval", json!(5)),
        ("interval", json!("60")),
        ("deadline_seconds", json!(0)),
        ("pr", json!(0)),
    ] {
        let mut bad = base_config();
        bad[key] = value;
        bad["workflows"] = json!([{"file": "ci.yml", "event": "push"}]);
        assert!(watch::validate(&bad).is_err(), "accepted {bad}");
    }
}

#[test]
fn result_code_matches_the_documented_states() {
    assert_eq!(watch::result_code("success"), 0);
    assert_eq!(watch::result_code("failure"), 1);
    for state in ["superseded", "api_error", "needs_attention"] {
        assert_eq!(watch::result_code(state), 2);
    }
    for state in ["pending", "running", "timeout", "unknown"] {
        assert_eq!(watch::result_code(state), 3);
    }
}

// ---------------------------------------------------------------------------
// External check rows
// ---------------------------------------------------------------------------

#[derive(Default)]
struct CheckApi {
    gets: BTreeMap<String, Value>,
    pages: BTreeMap<(String, String), Vec<Value>>,
}

impl CheckApi {
    fn with_window(mut self, count: i64) -> Self {
        self.gets.insert(
            format!("commits/{SHA}/check-suites?per_page=1"),
            json!({"total_count": count}),
        );
        self
    }

    fn with_runs(mut self, runs: Vec<Value>) -> Self {
        self.pages.insert(
            (
                format!("commits/{SHA}/check-runs?filter=all"),
                "check_runs".to_string(),
            ),
            runs,
        );
        self
    }

    fn with_check(mut self, check: Value) -> Self {
        self.gets.insert("check-runs/10".to_string(), check);
        self
    }
}

impl watch::GitHubApi for CheckApi {
    fn get(&self, suffix: &str) -> herdr_flow::Result<Value> {
        self.gets
            .get(suffix)
            .cloned()
            .ok_or_else(|| herdr_flow::FlowError::watch(format!("unexpected GET {suffix}")))
    }

    fn pages(&self, suffix: &str, key: &str) -> herdr_flow::Result<Vec<Value>> {
        self.pages
            .get(&(suffix.to_string(), key.to_string()))
            .cloned()
            .ok_or_else(|| herdr_flow::FlowError::watch(format!("unexpected PAGES {suffix}")))
    }
}

fn check_config() -> Value {
    json!({
        "sha": SHA,
        "checks": [{"name": "Trusted gate", "app_id": 123, "external_id": "gate-1"}]
    })
}

fn check_run(conclusion: Value) -> Value {
    json!({
        "id": 10,
        "name": "Trusted gate",
        "app": {"id": 123},
        "head_sha": SHA,
        "external_id": "gate-1",
        "status": "completed",
        "conclusion": conclusion,
        "details_url": "https://example/check/10"
    })
}

#[test]
fn check_rows_report_missing_and_identity_changes() {
    let api = CheckApi::default().with_window(1).with_runs(Vec::new());
    let rows = checks::run_rows(&check_config(), &api).unwrap();
    assert_eq!(model::string(&rows[0], "state").as_deref(), Some("pending"));
    assert_eq!(
        model::string(&rows[0], "reason").as_deref(),
        Some("external_check_missing")
    );
    assert!(!model::bool_or(&rows[0], "decision_terminal", true));

    let mut changed = check_run(json!("success"));
    changed["name"] = json!("Other gate");
    let api = CheckApi::default()
        .with_window(1)
        .with_runs(vec![check_run(json!("success"))])
        .with_check(changed);
    let rows = checks::run_rows(&check_config(), &api).unwrap();
    assert_eq!(
        model::string(&rows[0], "state").as_deref(),
        Some("needs_attention")
    );
    assert_eq!(
        model::string(&rows[0], "reason").as_deref(),
        Some("external_check_identity_changed")
    );
    assert!(model::bool_or(&rows[0], "decision_terminal", false));
}

#[test]
fn check_rows_report_success_failure_and_pending_status() {
    for (status, conclusion, state, reason, terminal) in [
        (
            "completed",
            json!("success"),
            "success",
            "external_check_success",
            true,
        ),
        (
            "completed",
            json!("failure"),
            "failure",
            "external_check_failure",
            true,
        ),
        (
            "in_progress",
            Value::Null,
            "pending",
            "external_check_pending",
            false,
        ),
    ] {
        let mut current = check_run(conclusion);
        current["status"] = json!(status);
        let api = CheckApi::default()
            .with_window(1)
            .with_runs(vec![check_run(json!("success"))])
            .with_check(current);
        let rows = checks::run_rows(&check_config(), &api).unwrap();
        assert_eq!(
            model::string(&rows[0], "state").as_deref(),
            Some(state),
            "{rows:?}"
        );
        assert_eq!(model::string(&rows[0], "reason").as_deref(), Some(reason));
        assert_eq!(
            model::bool_or(&rows[0], "decision_terminal", !terminal),
            terminal
        );
    }
}

#[test]
fn suite_window_overflow_short_circuits_check_rows() {
    let api = CheckApi::default().with_window(1001);
    let rows = checks::run_rows(&check_config(), &api).unwrap();
    assert_eq!(
        model::string(&rows[0], "reason").as_deref(),
        Some("external_check_suite_window_exceeded")
    );
}

// ---------------------------------------------------------------------------
// Metrics + watcher service
// ---------------------------------------------------------------------------

#[test]
fn empty_metrics_report_formats_without_tasks() {
    let tmp = TempDir::new().unwrap();
    util::atomic_write_json(&tmp.path().join("queue.json"), &json!({"id": "q"})).unwrap();
    let report = metrics::report(tmp.path(), Some(10.0)).unwrap();
    let text = metrics::format_report(&report);
    assert!(text.starts_with("queue=q | tasks=0"));
    assert!(text.contains("aggregate:"));
    assert!(text.contains("created_to_completed: n=0"));
    assert!(text.contains("capabilities:"));
}

#[test]
fn acquire_lock_retries_and_then_fails_while_held() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("watch.lock");
    assert!(watch_service::acquire_lock(&path, 0, Duration::from_millis(1)).is_err());
    let lock = watch_service::acquire_lock(&path, 3, Duration::from_millis(1)).unwrap();
    assert!(watch_service::acquire_lock(&path, 2, Duration::from_millis(1)).is_err());
    drop(lock);
    assert!(watch_service::acquire_lock(&path, 2, Duration::from_millis(1)).is_ok());
}
