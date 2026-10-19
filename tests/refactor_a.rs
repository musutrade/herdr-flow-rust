//! Behaviour-preservation tests for the decomposed guard entry points.

use std::path::Path;

use herdr_flow::guard;
use herdr_flow::model;
use serde_json::json;

fn args(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| value.to_string()).collect()
}

#[test]
fn help_exits_zero_and_unknown_arguments_exit_two() {
    assert_eq!(guard::cli(&args(&["--help"])), 0);
    assert_eq!(guard::cli(&args(&["-h"])), 0);
    assert_eq!(
        guard::cli(&args(&["status", "--state-dir", "/tmp/state", "--bogus"])),
        2
    );
}

#[test]
fn parse_validation_rejects_missing_and_invalid_options_without_touching_state() {
    assert_eq!(guard::cli(&args(&["status"])), 2);
    assert_eq!(
        guard::cli(&args(&[
            "status",
            "--state-dir",
            "/tmp/state",
            "--wait",
            "5"
        ])),
        2
    );
    assert_eq!(
        guard::cli(&args(&[
            "init",
            "--state-dir",
            "/tmp/state",
            "--pane",
            "w1:p1",
            "--session",
            "s",
            "--transcript",
            "/tmp/transcript.jsonl",
            "--task",
            "t",
            "--epoch",
            "not-a-number"
        ])),
        2
    );
    assert_eq!(
        guard::cli(&args(&[
            "status",
            "--state-dir",
            "/tmp/state",
            "--no-advisor"
        ])),
        2
    );
}

#[test]
fn advice_absent_reports_not_requested_or_disabled() {
    let not_requested = guard::advice_status(Path::new("/tmp"), &json!({})).unwrap();
    assert_eq!(
        model::string(&not_requested, "status").as_deref(),
        Some("not_requested")
    );
    let disabled =
        guard::advice_status(Path::new("/tmp"), &json!({"advisor_enabled": false})).unwrap();
    assert_eq!(
        model::string(&disabled, "status").as_deref(),
        Some("disabled")
    );
}

#[test]
fn review_events_absent_reports_not_observed() {
    let directory = tempfile::tempdir().unwrap();
    let events = guard::review_events(directory.path(), &json!({})).unwrap();
    assert_eq!(
        model::string(&events, "status").as_deref(),
        Some("not_observed")
    );
    assert!(model::get(&events, "events")
        .and_then(serde_json::Value::as_array)
        .is_some_and(Vec::is_empty));
}
