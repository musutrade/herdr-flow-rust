//! Regression coverage for the refactored queue CLI parser and dispatcher.

use herdr_flow::queue::cli;

fn args(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| value.to_string()).collect()
}

#[test]
fn cli_usage_errors_exit_two_without_touching_state() {
    assert_eq!(cli::cli(&args(&["--help"])), 0);
    assert_eq!(cli::cli(&args(&["status"])), 2);
    assert_eq!(cli::cli(&args(&["--bogus"])), 2);
    assert_eq!(
        cli::cli(&args(&["bogus", "--state-dir", "/tmp/refactor-g-state"])),
        2
    );
    assert_eq!(
        cli::cli(&args(&["signal", "--state-dir", "/tmp/refactor-g-state"])),
        2
    );
    assert_eq!(
        cli::cli(&args(&["ack", "--state-dir", "/tmp/refactor-g-state"])),
        2
    );
    assert_eq!(
        cli::cli(&args(&[
            "resolve",
            "--state-dir",
            "/tmp/refactor-g-state",
            "--delivery",
            "delivery-one"
        ])),
        2
    );
    assert_eq!(
        cli::cli(&args(&[
            "run",
            "--state-dir",
            "/tmp/refactor-g-state",
            "--wait",
            "wait-one"
        ])),
        2
    );
    assert_eq!(
        cli::cli(&args(&["task-add", "--state-dir", "/tmp/refactor-g-state"])),
        2
    );
    assert_eq!(
        cli::cli(&args(&[
            "event-current",
            "--state-dir",
            "/tmp/refactor-g-state"
        ])),
        2
    );
}

#[test]
fn cli_init_and_status_work_offline() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let root = tmp.path().join("queue");
    assert_eq!(
        cli::cli(&args(&[
            "init",
            "--state-dir",
            &root.display().to_string(),
            "--poll-seconds",
            "30"
        ])),
        0
    );
    assert!(root.join("queue.json").exists());
    assert_eq!(
        cli::cli(&args(&[
            "status",
            "--state-dir",
            &root.display().to_string(),
            "--json"
        ])),
        0
    );
}
