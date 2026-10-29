//! Exercise the new operator entry against the real isolated environment and
//! protected queue. The Unix listener is deliberately never asked to run work.
use super::*;
use crate::task_runtime::coordinator_handoff as handoff;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;

fn prepare(f: &mut Fixture) -> UnixListener {
    f.broker.kill().unwrap();
    f.broker.wait().unwrap();
    f.broker = Command::new("sleep")
        .arg("300")
        .env(task_env::MARKER_ENV, &f.owner.instance_id)
        .process_group(0)
        .spawn()
        .unwrap();
    let socket = f.temp.path().join("upstream.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();
    listener.set_nonblocking(true).unwrap();
    f.prepare(&socket);
    let env = f.active();
    let root = f.root();
    let guard = root.join("guard");
    task_env::isolation::runtime_namespace(&env, &guard).unwrap();
    let model = json!({"providerID":"openai","id":"fixture","reasoning_effort":"high"});
    let mut runtime = util::read_state_json(&root, "runtime.json").unwrap();
    runtime["phase"] = json!("started");
    runtime["model"] = model.clone();
    runtime["approval_policy"] = json!("on-request");
    runtime["role_models"] = json!({"developer":model,"reviewer":model,"recovery":model});
    util::atomic_write_json(&root.join("runtime.json"), &runtime).unwrap();
    let binding = json!({"version":2,"adapter":"codex","receiver_kind":"task-runtime",
        "task":"parent","role":"coordinator","phase":"monitoring","state_dir":guard,
        "runtime":root,"instance":env.instance_id,"cwd":f.checkout(),"session":SESSION,
        "model":model,"control_protocol":"queue-tool-v2","approval_policy":"on-request"});
    util::atomic_write_json(&guard.join("state.json"), &binding).unwrap();
    util::atomic_write_json(
        &root.join("receiver.json"),
        &json!({"phase":"bound","guard_dir":guard,"state":binding}),
    )
    .unwrap();
    let mut journal = util::read_state_json(&root, "upstream-session.json").unwrap();
    journal["session_configuration"] = json!({"model":"fixture","modelProvider":"openai",
        "reasoningEffort":"high","cwd":f.checkout(),"approvalPolicy":"on-request",
        "sandbox":{"type":"workspaceWrite","writableRoots":[f.checkout()],"networkAccess":false}});
    util::atomic_write_json(&root.join("upstream-session.json"), &journal).unwrap();
    let mut q = f.state();
    q["guard_dir"] = json!(guard);
    q["receiver"] = crate::guard::scope(&binding);
    f.write_state(&mut q);
    listener
}

fn request(f: &Fixture, id: &str) -> Result<J> {
    handoff::action(
        &f.root(),
        "parent",
        "receiver-handoff-request",
        &json!({"request_id":id,"reason":"explicit settled coordinator replacement"}),
    )
}

fn cancel(f: &Fixture, id: &str) -> Result<J> {
    handoff::action(
        &f.root(),
        "parent",
        "receiver-handoff-cancel",
        &json!({"request_id":id}),
    )
}

#[test]
fn real_environment_request_is_idempotent_fenced_and_cancellable_without_rpc() {
    let mut f = Fixture::new();
    let listener = prepare(&mut f);
    assert_eq!(
        handoff::status(&f.root(), "parent").unwrap()["phase"],
        "not_requested"
    );
    let before = f.state();
    let first = request(&f, "replacement-1").unwrap();
    assert_eq!(first["records"][0]["phase"], "draining");
    assert_eq!(request(&f, "replacement-1").unwrap(), first);
    let after = f.state();
    assert!(request(&f, "replacement-2").is_err());
    assert!(handoff::action(
        &f.root(),
        "parent",
        "receiver-handoff-request",
        &json!({"request_id":"replacement-1","reason":"changed"})
    )
    .is_err());
    assert_eq!(f.state(), after);
    assert!(!Path::new(first["records"][0]["to"]["runtime"].as_str().unwrap()).exists());
    assert!(!automatic_input_allowed(&f.root()).unwrap());
    assert!(check_workspace(&after, &f.checkout()).is_err());
    for field in [
        "requirements",
        "environment",
        "candidate",
        "approved",
        "verdicts",
        "operations",
        "epoch",
    ] {
        assert_eq!(
            after["tasks"]["parent"][field], before["tasks"]["parent"][field],
            "{field}"
        );
    }
    assert_eq!(after["receiver"], before["receiver"]);
    let cancelled = cancel(&f, "replacement-1").unwrap();
    assert_eq!(cancelled["records"][0]["phase"], "cancelled");
    assert_eq!(cancel(&f, "replacement-1").unwrap(), cancelled);
    assert_eq!(request(&f, "replacement-1").unwrap(), cancelled);
    assert!(automatic_input_allowed(&f.root()).unwrap());
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[test]
fn real_owner_capacity_and_unknown_resource_cannot_be_bypassed_by_new_id() {
    let mut f = Fixture::new();
    let listener = prepare(&mut f);
    for n in 1..=8 {
        let id = format!("replacement-{n}");
        let active = request(&f, &id).unwrap();
        assert_eq!(active["records"].as_array().unwrap().len(), n);
        cancel(&f, &id).unwrap();
    }
    let before = f.state();
    assert!(request(&f, "replacement-9").is_err());
    assert_eq!(f.state(), before);
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}
