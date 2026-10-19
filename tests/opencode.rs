//! OpenCode 1.19.3 adapter behavior and diagnostic CLI tests.

pub mod common;

use std::cell::RefCell;
use std::collections::VecDeque;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::rc::Rc;
use std::time::Duration;

use herdr_flow::error::ErrorKind;
use herdr_flow::model::J;
use herdr_flow::opencode::{baseline_capabilities, create_opencode_adapter, OpenCodeAdapter};
use serde_json::{json, Value};

fn state() -> J {
    json!({
        "pane": "w1:p1",
        "shell_pid": 12345,
        "cwd": "/home/test/repo",
        "terminal": "term1"
    })
}

fn json_reply(value: Value) -> Vec<u8> {
    serde_json::to_vec(&value).expect("test reply")
}

fn identity_ok(pid: u32) -> herdr_flow::Result<J> {
    Ok(json!({"pid": pid, "start_ticks": "1234567890"}))
}

fn adapter_with<F>(
    state: &J,
    replies: Vec<Vec<u8>>,
    identity: F,
) -> (OpenCodeAdapter<'static>, Rc<RefCell<Vec<Vec<String>>>>)
where
    F: Fn(u32) -> herdr_flow::Result<J> + 'static,
{
    let replies = Rc::new(RefCell::new(
        replies.into_iter().collect::<VecDeque<Vec<u8>>>(),
    ));
    let calls = Rc::new(RefCell::new(Vec::<Vec<String>>::new()));
    let runner_replies = replies;
    let runner_calls = calls.clone();
    let runner = move |argv: &[String], timeout: Duration, transient: bool| {
        assert_eq!(timeout, Duration::from_secs(30));
        assert!(transient, "call() is always a transient command");
        runner_calls.borrow_mut().push(argv.to_vec());
        runner_replies
            .borrow_mut()
            .pop_front()
            .ok_or_else(|| herdr_flow::FlowError::guard("test runner exhausted"))
    };
    (OpenCodeAdapter::new(state, runner, identity), calls)
}

fn pane_reply(terminal: &str) -> Vec<u8> {
    json_reply(json!({"result": {"pane": {"terminal_id": terminal}}}))
}

fn process_reply(shell_pid: u64, group: u64, foreground: Vec<u64>) -> Vec<u8> {
    let foreground: Vec<Value> = foreground.iter().map(|pid| json!({"pid": pid})).collect();
    json_reply(json!({
        "result": {
            "process_info": {
                "shell_pid": shell_pid,
                "foreground_process_group_id": group,
                "foreground_processes": foreground
            }
        }
    }))
}

fn shell_ready_replies() -> Vec<Vec<u8>> {
    vec![
        pane_reply("term1"),
        process_reply(12345, 12345, vec![12345]),
    ]
}

fn guard_message(error: herdr_flow::FlowError) -> String {
    assert_eq!(error.kind, ErrorKind::Guard);
    error.message
}

#[test]
fn capabilities_match_the_python_baseline() {
    let (adapter, calls) = adapter_with(&state(), Vec::new(), identity_ok);
    let caps = adapter.check_capabilities().unwrap();
    assert_eq!(caps, baseline_capabilities());
    assert_eq!(caps["adapter"], "OpenCode 1.19.3");
    assert_eq!(caps["available"], true);
    assert_eq!(
        caps["server_info"],
        json!({"name": "opencode", "version": "1.19.3"})
    );
    assert_eq!(
        caps["missing"][0],
        "app-server RPC (no thread/read, thread/resume, thread/settings/update)"
    );
    assert_eq!(
        caps["hints"][0],
        "OpenCode session continuity: shell PID + process identity only"
    );
    assert_eq!(
        caps["supported_operations"][3],
        "successor session start (new opencode instance)"
    );
    assert_eq!(caps["missing"].as_array().unwrap().len(), 5);
    assert_eq!(caps["hints"].as_array().unwrap().len(), 4);
    assert_eq!(caps["supported_operations"].as_array().unwrap().len(), 4);
    // Cached, and capability reporting never touches the Herdr CLI.
    assert_eq!(adapter.check_capabilities().unwrap(), caps);
    assert!(calls.borrow().is_empty());
}

#[test]
fn pane_verifies_the_registered_terminal() {
    let (adapter, calls) = adapter_with(&state(), vec![pane_reply("term1")], identity_ok);
    let pane = adapter.pane().unwrap();
    assert_eq!(pane["terminal_id"], "term1");
    assert_eq!(
        calls.borrow()[0],
        vec!["herdr", "pane", "get", "w1:p1"]
            .into_iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
    );
}

#[test]
fn pane_rejects_a_terminal_change() {
    let (adapter, _) = adapter_with(&state(), vec![pane_reply("term2")], identity_ok);
    let message = guard_message(adapter.pane().unwrap_err());
    assert!(message.contains("terminal changed"), "{message}");
}

#[test]
fn shell_ready_when_only_the_shell_is_in_foreground() {
    let (adapter, _) = adapter_with(&state(), shell_ready_replies(), identity_ok);
    assert!(adapter.shell_ready().unwrap());

    let (adapter, _) = adapter_with(&state(), shell_ready_replies(), identity_ok);
    assert!(adapter.idle().unwrap());
}

#[test]
fn shell_ready_accepts_empty_foreground_when_group_matches() {
    let replies = vec![pane_reply("term1"), process_reply(12345, 12345, Vec::new())];
    let (adapter, _) = adapter_with(&state(), replies, identity_ok);
    assert!(adapter.shell_ready().unwrap());
}

#[test]
fn shell_not_ready_with_a_foreground_process() {
    let replies = vec![
        pane_reply("term1"),
        process_reply(12345, 99999, vec![99999]),
    ];
    let (adapter, _) = adapter_with(&state(), replies, identity_ok);
    let message = guard_message(adapter.shell_ready().unwrap_err());
    assert!(message.contains("shell not idle"), "{message}");
}

#[test]
fn shell_ready_rejects_a_pane_shell_change() {
    let replies = vec![
        pane_reply("term1"),
        process_reply(54321, 54321, vec![54321]),
    ];
    let (adapter, _) = adapter_with(&state(), replies, identity_ok);
    let message = guard_message(adapter.shell_ready().unwrap_err());
    assert!(message.contains("pane shell changed"), "{message}");
}

#[test]
fn agent_status_is_always_unknown() {
    let (adapter, _) = adapter_with(&state(), vec![pane_reply("term1")], identity_ok);
    let agent = adapter.agent().unwrap();
    assert_eq!(agent["agent_session"]["value"], Value::Null);
    assert_eq!(agent["agent_status"], "unknown");
    assert_eq!(agent["interactive_ready"], false);
}

#[test]
fn marker_is_always_absent() {
    let (adapter, calls) = adapter_with(&state(), Vec::new(), identity_ok);
    assert_eq!(adapter.marker().unwrap(), None);
    assert!(calls.borrow().is_empty());
}

#[test]
fn verify_tui_checks_the_shell_identity_only() {
    let (adapter, _) = adapter_with(&state(), shell_ready_replies(), identity_ok);
    let identity = adapter.verify_tui("any-session").unwrap();
    assert_eq!(identity["pid"], 12345);
    assert_eq!(identity["start_ticks"], "1234567890");
}

#[test]
fn verify_tui_rejects_a_pane_shell_change() {
    let replies = vec![
        pane_reply("term1"),
        process_reply(54321, 54321, vec![54321]),
    ];
    let (adapter, _) = adapter_with(&state(), replies, identity_ok);
    let message = guard_message(adapter.verify_tui("session").unwrap_err());
    assert!(message.contains("pane shell changed"), "{message}");
}

#[test]
fn verify_tui_rejects_a_recreated_shell_process() {
    let (adapter, _) = adapter_with(&state(), shell_ready_replies(), |_pid| {
        Ok(json!({"pid": 777, "start_ticks": "1"}))
    });
    let message = guard_message(adapter.verify_tui("session").unwrap_err());
    assert!(message.contains("shell process died"), "{message}");
}

#[test]
fn unsupported_operations_keep_guard_error_boundaries() {
    let (adapter, calls) = adapter_with(&state(), Vec::new(), identity_ok);

    let message = guard_message(adapter.snapshot(None).unwrap_err());
    assert!(
        message.contains("does not support runtime snapshots"),
        "{message}"
    );

    let message = guard_message(adapter.rpc("thread/read", &json!({})).unwrap_err());
    assert_eq!(
        message,
        "OpenCode 1.19.3 does not support RPC method: thread/read"
    );

    let message = guard_message(adapter.prompt("hello").unwrap_err());
    assert_eq!(
        message,
        "OpenCode 1.19.3 prompt requires manual input or file-based handoff"
    );

    let message = guard_message(adapter.exit_tui().unwrap_err());
    assert_eq!(
        message,
        "OpenCode 1.19.3 exit requires manual Ctrl+C or process signal"
    );

    let message = guard_message(
        adapter
            .restore_runtime(&json!({"model": "x"}), "session")
            .unwrap_err(),
    );
    assert_eq!(
        message,
        "OpenCode 1.19.3 does not support runtime restoration"
    );

    // None of the unsupported operations may invoke the Herdr CLI.
    assert!(calls.borrow().is_empty());
}

#[test]
fn launch_successor_requires_an_idle_shell_and_is_not_automated() {
    let (adapter, _) = adapter_with(&state(), shell_ready_replies(), identity_ok);
    let message = guard_message(
        adapter
            .launch_successor(Path::new("/root"), Path::new("/handoff.json"))
            .unwrap_err(),
    );
    assert!(
        message.contains("successor launch not automated"),
        "{message}"
    );

    let replies = vec![
        pane_reply("term1"),
        process_reply(12345, 99999, vec![99999]),
    ];
    let (adapter, _) = adapter_with(&state(), replies, identity_ok);
    let message = guard_message(
        adapter
            .launch_successor(Path::new("/root"), Path::new("/handoff.json"))
            .unwrap_err(),
    );
    assert!(message.contains("shell not idle"), "{message}");
}

#[test]
fn call_unwraps_result_and_rejects_herdr_errors() {
    let (adapter, _) = adapter_with(
        &state(),
        vec![json_reply(json!({"result": {"ok": true}}))],
        identity_ok,
    );
    assert_eq!(
        adapter.call(&["status".to_string()]).unwrap(),
        json!({"ok": true})
    );

    let (adapter, _) = adapter_with(&state(), vec![json_reply(json!({"ok": true}))], identity_ok);
    assert_eq!(
        adapter.call(&["status".to_string()]).unwrap(),
        json!({"ok": true})
    );

    let (adapter, _) = adapter_with(
        &state(),
        vec![json_reply(json!({"error": {"message": "denied"}}))],
        identity_ok,
    );
    let message = guard_message(adapter.call(&["status".to_string()]).unwrap_err());
    assert_eq!(message, "Herdr rejected the command");
}

#[test]
fn call_treats_empty_pane_run_output_as_an_empty_result() {
    let (adapter, _) = adapter_with(&state(), vec![b" \n".to_vec()], identity_ok);
    let result = adapter
        .call(&["pane".to_string(), "run".to_string(), "w1:p1".to_string()])
        .unwrap();
    assert_eq!(result, json!({}));

    let (adapter, _) = adapter_with(&state(), vec![Vec::new()], identity_ok);
    let message = guard_message(adapter.call(&["status".to_string()]).unwrap_err());
    assert!(message.contains("invalid JSON"), "{message}");
}

#[test]
fn factory_uses_the_standard_boundaries() {
    let adapter = create_opencode_adapter(&state()).unwrap();
    assert_eq!(adapter.state, state());
    assert_eq!(
        adapter.check_capabilities().unwrap()["adapter"],
        "OpenCode 1.19.3"
    );
}

#[test]
fn adapter_accepts_owned_state_and_text_runner() {
    let adapter = OpenCodeAdapter::new(
        state(),
        |_argv: &[String], _timeout: Duration, _transient: bool| {
            Ok("{\"result\":{\"pane\":{\"terminal_id\":\"term1\"}}}".to_string())
        },
        identity_ok,
    );
    assert_eq!(adapter.pane().unwrap()["terminal_id"], "term1");
}

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_herdr-flow")
}

#[test]
fn cli_capabilities_prints_the_baseline() {
    let output = std::process::Command::new(binary())
        .args(["opencode", "capabilities"])
        .output()
        .expect("run opencode capabilities");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let printed: Value = serde_json::from_slice(&output.stdout).expect("baseline JSON");
    assert_eq!(printed, baseline_capabilities());

    let tmp = common::tempdir().unwrap();
    let output = std::process::Command::new(binary())
        .args([
            "opencode",
            "capabilities",
            "--state-dir",
            tmp.path().to_str().unwrap(),
        ])
        .output()
        .expect("run opencode capabilities with state dir");
    assert!(output.status.success());
    let printed: Value = serde_json::from_slice(&output.stdout).expect("baseline JSON");
    assert_eq!(printed, baseline_capabilities());
}

#[test]
fn cli_errors_use_error_type_names_and_exit_two() {
    for args in [
        vec!["opencode"],
        vec!["opencode", "nope"],
        vec!["opencode", "verify", "--state-dir", "/tmp"],
        vec!["opencode", "verify", "--shell-pid", "not-a-pid"],
    ] {
        let output = std::process::Command::new(binary())
            .args(&args)
            .output()
            .expect("run opencode CLI");
        assert_eq!(output.status.code(), Some(2), "args: {args:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.starts_with("ValueError: "),
            "args: {args:?}, stderr: {stderr}"
        );
    }
}

#[cfg(unix)]
#[test]
fn cli_verify_checks_pane_process_and_shell_identity_read_only() {
    let tmp = common::tempdir().unwrap();
    let bin = tmp.path().join("bin");
    let state_dir = tmp.path().join("state");
    common::create_dir_all(&bin).unwrap();
    common::create_dir_all(&state_dir).unwrap();
    let log = tmp.path().join("herdr.log");
    let pid = std::process::id();
    let herdr = bin.join("herdr");
    let script = format!(
        "#!/bin/sh\n\
         printf '%s\\n' \"$*\" >> \"$HERDR_LOG\"\n\
         case \"$1 $2\" in\n\
           'pane get') printf '%s\\n' '{{\"result\":{{\"pane\":{{\"terminal_id\":\"term1\"}}}}}}' ;;\n\
           'pane process-info') printf '%s\\n' '{{\"result\":{{\"process_info\":{{\"shell_pid\":{pid},\"foreground_process_group_id\":{pid},\"foreground_processes\":[{{\"pid\":{pid}}}]}}}}}}' ;;\n\
           *) printf '%s\\n' '{{\"error\":\"unexpected\"}}'; exit 1 ;;\n\
         esac\n"
    );
    std::fs::write(&herdr, script).unwrap();
    let mut permissions = std::fs::metadata(&herdr).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&herdr, permissions).unwrap();

    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let output = std::process::Command::new(binary())
        .args([
            "opencode",
            "verify",
            "--state-dir",
            state_dir.to_str().unwrap(),
            "--pane",
            "w1:p1",
            "--terminal",
            "term1",
            "--shell-pid",
            &pid.to_string(),
        ])
        .env("PATH", path)
        .env("HERDR_LOG", &log)
        .output()
        .expect("run opencode verify");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let printed: Value = serde_json::from_slice(&output.stdout).expect("verify JSON");
    assert_eq!(printed["pane"]["terminal_id"], "term1");
    assert_eq!(printed["shell_identity"]["pid"], pid);
    assert_eq!(printed["process_info"]["shell_pid"], pid);

    let calls = std::fs::read_to_string(&log).unwrap();
    assert!(calls.contains("pane get w1:p1"), "{calls}");
    assert!(calls.contains("pane process-info --pane w1:p1"), "{calls}");
    assert!(!calls.contains("pane run"), "{calls}");
    assert!(!calls.contains("prompt"), "{calls}");
}

#[cfg(unix)]
#[test]
fn cli_verify_reports_guard_errors() {
    let tmp = common::tempdir().unwrap();
    let bin = tmp.path().join("bin");
    let state_dir = tmp.path().join("state");
    common::create_dir_all(&bin).unwrap();
    common::create_dir_all(&state_dir).unwrap();
    let pid = std::process::id();
    let herdr = bin.join("herdr");
    let script = "#!/bin/sh\n\
         printf '%s\\n' '{\"result\":{\"pane\":{\"terminal_id\":\"term1\"}}}'\n\
         exit 0\n";
    std::fs::write(&herdr, script).unwrap();
    let mut permissions = std::fs::metadata(&herdr).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&herdr, permissions).unwrap();

    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let output = std::process::Command::new(binary())
        .args([
            "opencode",
            "verify",
            "--state-dir",
            state_dir.to_str().unwrap(),
            "--pane",
            "w1:p1",
            "--terminal",
            "different",
            "--shell-pid",
            &pid.to_string(),
        ])
        .env("PATH", path)
        .output()
        .expect("run opencode verify");
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.starts_with("GuardError: pane terminal changed"),
        "{stderr}"
    );
}

#[cfg(unix)]
#[test]
fn cli_verify_preserves_guard_and_transient_error_boundaries() {
    let tmp = common::tempdir().unwrap();
    let bin = tmp.path().join("bin");
    let state_dir = tmp.path().join("state");
    common::create_dir_all(&bin).unwrap();
    common::create_dir_all(&state_dir).unwrap();
    let pid = std::process::id();
    let herdr = bin.join("herdr");
    let script = "#!/bin/sh\n\
         case \"$FAKE_MODE\" in\n\
           envelope) printf '%s\\n' '{\"error\":{\"message\":\"rejected\"}}'; exit 0 ;;\n\
           *) printf '%s\\n' 'denied /home/alice/repo https://bob:secret@example.com/x abcdefghijklmnopqrstuvwxyz012345' >&2; exit 1 ;;\n\
         esac\n";
    std::fs::write(&herdr, script).unwrap();
    let mut permissions = std::fs::metadata(&herdr).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&herdr, permissions).unwrap();

    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let run = |mode: &str| {
        std::process::Command::new(binary())
            .args([
                "opencode",
                "verify",
                "--state-dir",
                state_dir.to_str().unwrap(),
                "--pane",
                "w1:p1",
                "--terminal",
                "term1",
                "--shell-pid",
                &pid.to_string(),
            ])
            .env("PATH", &path)
            .env("FAKE_MODE", mode)
            .output()
            .expect("run opencode verify")
    };

    // A non-zero herdr exit is transient, exactly as in the Python runner.
    let output = run("transient");
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.starts_with("TransientError: "), "{stderr}");
    assert!(stderr.contains("[REDACTED]"), "{stderr}");
    assert!(stderr.contains("[USER]"), "{stderr}");
    assert!(stderr.contains("[TOKEN]"), "{stderr}");
    assert!(!stderr.contains("alice"), "{stderr}");
    assert!(!stderr.contains("bob:secret"), "{stderr}");

    // A herdr error envelope is a GuardError even on exit status zero.
    let output = run("envelope");
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.starts_with("GuardError: Herdr rejected the command"),
        "{stderr}"
    );
}
