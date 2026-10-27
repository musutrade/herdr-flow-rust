//! Removed adapters must fail before contacting a service or changing old state.
pub mod common;
use herdr_flow::{herdr, identity, queue, task_runtime, util};
use serde_json::json;
use std::process::Command;

#[test]
fn removed_command_and_configuration_are_rejected() {
    let executable = env!("CARGO_BIN_EXE_herdr-flow");
    let help = Command::new(executable).arg("--help").output().unwrap();
    assert!(!String::from_utf8_lossy(&help.stdout).contains("opencode"));
    for args in [
        vec!["opencode", "--help"],
        vec!["opencode", "watch"],
        vec![
            "task-runtime",
            "configuration-status",
            "--adapter",
            "opencode",
        ],
    ] {
        assert_eq!(
            Command::new(executable)
                .args(args)
                .output()
                .unwrap()
                .status
                .code(),
            Some(2)
        );
    }
    assert!(task_runtime::configuration::status("opencode").is_err());
    assert!(identity::validate(&json!({"adapter":"opencode","session":"old"})).is_err());
}

#[test]
fn old_receiver_cannot_borrow_codex_identity_or_modify_its_queue() {
    let temp = common::tempdir().unwrap();
    let guard = temp.path().join("guard");
    let root = temp.path().join("queue");
    util::ensure_private_dir(&guard, true).unwrap();
    let cwd = temp.path().join("checkout");
    std::fs::create_dir(&cwd).unwrap();
    let state = json!({"adapter":"opencode","session":"old","cwd":cwd,
        "endpoint":"http://127.0.0.1:1","task":"old","role":"coordinator"});
    util::atomic_write_json(&guard.join("state.json"), &state).unwrap();
    queue::init(&root, Some(&guard), 30).unwrap();
    let before = std::fs::read(root.join("queue.json")).unwrap();
    let registration = std::fs::read(guard.join("state.json")).unwrap();
    assert!(herdr::receiver_factory(&state).is_err());
    assert!(herdr::Herdr::new(&state).is_err());
    assert!(identity::receiver_session(&root).is_err());
    let result = Command::new(env!("CARGO_BIN_EXE_herdr-flow"))
        .args(["queue", "ack", "--state-dir"])
        .arg(&root)
        .args(["--delivery", "old-delivery"])
        .env("CODEX_THREAD_ID", "old")
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert_eq!(std::fs::read(root.join("queue.json")).unwrap(), before);
    assert_eq!(
        std::fs::read(guard.join("state.json")).unwrap(),
        registration
    );
}
