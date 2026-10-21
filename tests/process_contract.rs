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
