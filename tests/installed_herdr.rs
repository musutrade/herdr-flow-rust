//! Opt-in, read-only acceptance for the real Herdr process boundary.
use std::process::Command;

use herdr_flow::herdr::{GuardAdapter, Herdr};
use serde_json::{json, Value};

fn query(args: &[&str]) -> Value {
    let output = Command::new("herdr").args(args).output().unwrap();
    assert!(output.status.success());
    serde_json::from_slice::<Value>(&output.stdout).unwrap()["result"].clone()
}

#[test]
#[ignore = "requires a Herdr-managed test context; queries only"]
fn installed_herdr_read_only_binding() {
    assert_eq!(std::env::var("HERDR_ENV").unwrap(), "1");
    let pane = std::env::var("HERDR_PANE_ID").unwrap();
    let pane_info = query(&["pane", "get", &pane]);
    let process_info = query(&["pane", "process-info", "--pane", &pane]);
    let mut state = json!({
        "version":2,"socket":std::env::var("HERDR_SOCKET_PATH").unwrap(),
        "pane":pane,"terminal":pane_info["pane"]["terminal_id"],
        "shell_pid":process_info["process_info"]["shell_pid"]
    });
    let adapter = Herdr::new(&state).unwrap();
    assert_eq!(adapter.pane().unwrap()["terminal_id"], state["terminal"]);
    assert_eq!(
        adapter.processes().unwrap()["shell_pid"],
        state["shell_pid"]
    );
    state["terminal"] = json!("deliberately-wrong-acceptance-binding");
    assert!(Herdr::new(&state).unwrap().pane().is_err());
    let version = Command::new("herdr").arg("--version").output().unwrap();
    assert!(version.status.success());
    println!("{}: production CLI pane/process binding and terminal mismatch rejection passed (queries only)",String::from_utf8_lossy(&version.stdout).trim());
}
