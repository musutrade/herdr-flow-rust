//! Isolated deployment-boundary probe; no service or credential access.
pub mod common;
use herdr_flow::batch::gate;
use serde_json::json;
use std::{fs, os::unix::fs::PermissionsExt};

#[test]
fn writable_shebang_interpreter_is_rejected_before_execution() {
    let root = common::tempdir().unwrap();
    let interpreter = root.path().join("interpreter");
    let script = root.path().join("client");
    fs::write(&interpreter, "#!/bin/sh\nprintf '{}'\n").unwrap();
    fs::set_permissions(&interpreter, fs::Permissions::from_mode(0o777)).unwrap();
    fs::write(&script, format!("#!{}\n", interpreter.display())).unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
    let result = gate::call(&json!({"verifier":[script]}), &json!({"action":"verify"}));
    assert!(result.is_err(), "writable interpreter accepted: {result:?}");
}
