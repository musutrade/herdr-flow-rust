//! Exercise the production adapter through an actual CLI child and Unix RPC.
pub mod common;
#[path = "common/rpc_server.rs"]
mod server;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

use herdr_flow::error::ErrorKind;
use herdr_flow::herdr::{GuardAdapter, Herdr};
use herdr_flow::util;
use serde_json::{json, Value};
use server::Server;

const SESSION: &str = "11111111-1111-4111-8111-111111111111";

fn write(path: &std::path::Path, name: &str, value: &Value) {
    fs::write(path.join(name), serde_json::to_vec(value).unwrap()).unwrap();
}

fn state(root: &std::path::Path) -> Value {
    let pid = std::process::id();
    write(
        root,
        "pane.json",
        &json!({"result":{"pane":{"terminal_id":"terminal"}}}),
    );
    write(
        root,
        "process.json",
        &json!({"result":{"process_info":{
            "shell_pid":42,"foreground_process_group_id":pid,"foreground_processes":[{"pid":pid}]
        }}}),
    );
    json!({"version":2,"socket":"fixture-herdr","pane":"w1:p1",
        "terminal":"terminal","shell_pid":42,"session":SESSION,"cwd":"/fixture",
        "state_dir":root,"tui_process":util::process_identity(pid).unwrap()})
}

fn adapter(root: &std::path::Path, peer_thread: Value) -> (Herdr, Server) {
    let server = Server::start(move |mut peer| {
        peer.initialize();
        let request = peer.request();
        assert_eq!(request["method"], "thread/read");
        assert_eq!(request["params"]["threadId"], SESSION);
        peer.json(&json!({"id":request["id"],"result":{"thread":peer_thread}}));
    });
    let mut state = state(root);
    state["app_server_socket"] = json!(server.path);
    (Herdr::new(&state).unwrap(), server)
}

fn child(root: &std::path::Path) {
    let (agent, server) = adapter(
        root,
        json!({"id":SESSION,"cwd":"/fixture","status":{"type":"idle"},"canAcceptDirectInput":true}),
    );
    assert_eq!(
        agent.agent().unwrap(),
        json!({"agent_session":{"value":SESSION},"agent_status":"idle","interactive_ready":true})
    );
    server.finish();
    for thread in [
        json!({"id":"another","cwd":"/fixture","status":{"type":"idle"}}),
        json!({"id":SESSION,"cwd":"/wrong","status":{"type":"idle"}}),
        json!({"id":SESSION,"cwd":"/fixture","status":{"type":"unknown"}}),
    ] {
        let (agent, server) = adapter(root, thread);
        assert_eq!(agent.agent().unwrap_err().kind, ErrorKind::Guard);
        server.finish();
    }
    let (agent, server) = adapter(
        root,
        json!({"id":SESSION,"cwd":"/fixture","status":{"type":"active"}}),
    );
    assert_eq!(
        agent.prompt("must not send").unwrap_err().kind,
        ErrorKind::AgentBusy
    );
    server.finish();

    let agent = Herdr::new(&state(root)).unwrap();
    write(
        root,
        "pane.json",
        &json!({"result":{"pane":{"terminal_id":"wrong"}}}),
    );
    assert!(agent.pane().unwrap_err().message.contains("terminal"));
    state(root);
    write(
        root,
        "process.json",
        &json!({"result":{"process_info":{"shell_pid":999}}}),
    );
    assert!(agent
        .verify_tui(SESSION)
        .unwrap_err()
        .message
        .contains("shell"));
    state(root);
    write(
        root,
        "process.json",
        &json!({"result":{"process_info":{"shell_pid":42,"foreground_processes":[]}}}),
    );
    assert!(agent
        .verify_tui(SESSION)
        .unwrap_err()
        .message
        .contains("foreground"));

    assert!(agent
        .call(&["bad".into(), "json".into()])
        .unwrap_err()
        .message
        .contains("JSON"));
    assert!(agent.call(&["rejected".into(), "request".into()]).is_err());
    assert_eq!(
        agent.call(&["missing".into()]).unwrap_err().kind,
        ErrorKind::Transient
    );
    assert_eq!(
        agent.call(&["pane".into(), "run".into()]).unwrap(),
        json!({})
    );
    let calls = fs::read(root.join("calls")).unwrap();
    assert!(agent.call(&["pane;touch unexpected".into()]).is_err());
    assert_eq!(fs::read(root.join("calls")).unwrap(), calls);
    let text = String::from_utf8(calls).unwrap();
    assert!(text.contains("pane get w1:p1\n"));
    assert!(text.contains("pane process-info --pane w1:p1\n"));
}

#[test]
fn production_adapter_process_and_rpc_boundaries() {
    if let Some(root) = std::env::var_os("HERDR_ADAPTER_FIXTURE") {
        child(std::path::Path::new(&root));
        return;
    }
    let temp = common::tempdir().unwrap();
    let executable = temp.path().join("herdr");
    fs::write(
        &executable,
        r##"#!/bin/sh
printf '%s\n' "$*" >> "$HERDR_ADAPTER_FIXTURE/calls"
case "$1 $2" in
  'pane get') cat "$HERDR_ADAPTER_FIXTURE/pane.json" ;;
  'pane process-info') cat "$HERDR_ADAPTER_FIXTURE/process.json" ;;
  'bad json') printf 'invalid-json' ;;
  'rejected request') printf '{"error":{"message":"rejected"}}' ;;
  'pane run') exit 0 ;;
  *) exit 9 ;;
esac
"##,
    )
    .unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    let path = format!(
        "{}:{}",
        temp.path().display(),
        std::env::var("PATH").unwrap()
    );
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "production_adapter_process_and_rpc_boundaries",
            "--nocapture",
        ])
        .env("HERDR_ADAPTER_FIXTURE", temp.path())
        .env("HERDR_ENV", "1")
        .env("HERDR_SOCKET_PATH", "fixture-herdr")
        .env("PATH", path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
