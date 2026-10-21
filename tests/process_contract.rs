//! Public process helpers must reject invalid input before creating resources.
pub mod common;
use herdr_flow::{util::process, ErrorKind};
use std::time::Duration;

#[test]
fn empty_spawn_argv_returns_usage_without_creating_logs() {
    let root = common::tempdir().unwrap();
    let log = root.path().join("not-created/log");
    let ready = process::spawn_with_ready(&[], &log, None, Duration::from_secs(1));
    assert!(matches!(ready, Err(error) if error.kind == ErrorKind::Usage));
    let detached = process::spawn_detached(&[], &log, None);
    assert!(matches!(detached, Err(error) if error.kind == ErrorKind::Usage));
    let inherit = process::spawn_inherit(&[], None);
    assert!(matches!(inherit, Err(error) if error.kind == ErrorKind::Usage));
    assert!(!log.parent().unwrap().exists());
}

#[test]
fn readiness_pipe_owners_close_on_spawn_failure_timeout_and_success() {
    const CHILD: &str = "HERDR_TEST_READY_FD_OWNERS";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "readiness_pipe_owners_close_on_spawn_failure_timeout_and_success",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        return;
    }
    // A single-test subprocess prevents sibling tests' open files from affecting
    // counts. No external service or user process is inspected or signalled.
    let root = common::tempdir().unwrap();
    let log = root.path().join("helper.log");
    let count = || std::fs::read_dir("/proc/self/fd").unwrap().count();
    let before = count();
    let missing = vec![root.path().join("missing").to_string_lossy().into_owned()];
    assert!(process::spawn_with_ready(&missing, &log, None, Duration::from_millis(100)).is_err());
    assert_eq!(count(), before);
    let silent = vec!["/bin/sh".into(), "-c".into(), "while :; do :; done".into()];
    assert!(process::spawn_with_ready(&silent, &log, None, Duration::from_millis(20)).is_err());
    assert_eq!(count(), before);
    let valid = vec![
        "/bin/sh".into(),
        "-c".into(),
        "printf '{\"ready\":true}' >&3".into(),
    ];
    let mut child = process::spawn_with_ready(&valid, &log, None, Duration::from_secs(2)).unwrap();
    assert_eq!(child.ready.unwrap()["ready"], true);
    child.child.wait().unwrap();
    assert_eq!(count(), before);
}
