use super::*;
use std::os::fd::AsRawFd;

fn prepared(f: &Fixture) -> PathBuf {
    let env = f.active();
    let root = super::super::super::super::prepare(&env, "prepared-recovery").unwrap();
    let mut runtime = util::read_state_json(&f.root(), "runtime.json").unwrap();
    runtime["root"] = json!(root);
    runtime["recovery_role"] = json!(true);
    runtime["native_tui"] = json!({});
    runtime["model"] = json!({"providerID":"existing","id":"diagnostic","reasoning_effort":"high"});
    util::atomic_write_json(&root.join("runtime.json"), &runtime).unwrap();
    upstream::before(&root, "thread/start", &json!({"cwd":f.checkout()})).unwrap();
    upstream::after(&root, "thread/start", &json!({"thread":{"id":OTHER}})).unwrap();
    let observer = f.root().join("recovery-observer");
    let mut context = util::read_state_json(&observer, "observer.json").unwrap();
    context["runtime"] = json!({"native_tui":runtime["native_tui"]});
    util::atomic_write_json(&observer.join("observer.json"), &context).unwrap();
    util::atomic_write_json(
        &observer.join("agent.json"),
        &json!({"phase":"ready","runtime":root,"session":OTHER,"model":runtime["model"]}),
    )
    .unwrap();
    root
}

fn metadata_service(f: &Fixture) -> rpc_fixture::Server {
    let cwd = f.checkout();
    rpc_fixture::Server::start_connections(2, move |mut peer| {
        peer.initialize();
        let mut byte = [0u8];
        // SAFETY: peer owns the fd; byte is writable. This fixture's inherited
        // read timeout bounds malformed/incomplete protocol work.
        while unsafe {
            libc::recv(
                peer.stream.as_raw_fd(),
                byte.as_mut_ptr().cast(),
                1,
                libc::MSG_PEEK,
            )
        } > 0
        {
            let (opcode, payload) = peer.frame();
            if opcode == 8 {
                break;
            }
            assert_eq!(opcode, 1);
            let request: J = serde_json::from_slice(&payload).unwrap();
            let result = match request["method"].as_str().unwrap() {
                "thread/read" => {
                    json!({"thread":{"id":request["params"]["threadId"],"cwd":cwd,"status":{"type":"idle"}}})
                }
                "thread/backgroundTerminals/list" => json!({"data":[],"nextCursor":null}),
                "thread/loaded/list" => json!({"data":[SESSION,OTHER],"nextCursor":null}),
                method => panic!("unexpected mutation {method}"),
            };
            peer.json(&json!({"id":request["id"],"result":result}));
        }
    })
}

#[test]
fn fresh_recovery_creation_publishes_never_requested_presentation_atomically() {
    let (f, service) = Fixture::with_server(0, "idle");
    let root = prepared(&f);
    let journal = util::read_state_json(&root, "upstream-session.json").unwrap();
    assert_eq!(journal["phase"], "created");
    assert_eq!(journal["session"], OTHER);
    assert_eq!(journal["presentation_intent"]["phase"], "not_requested");
    service.finish();
}

#[test]
fn configured_prepared_recovery_keeps_human_quiescence_and_generic_retirement_strict() {
    let f = Fixture::new();
    let service = metadata_service(&f);
    f.prepare(&service.path);
    let root = prepared(&f);
    {
        let env = f.active();
        assert!(crate::task_runtime::tui::retirement_snapshot(&env, &root).is_err());
    }
    f.request();
    let acquired = f.acquire();
    assert_eq!(acquired["phase"], "human", "{acquired}");
    assert_eq!(acquired["proof"]["sessions"].as_array().unwrap().len(), 2);
    service.finish();
}

#[test]
fn prepared_recovery_requires_exact_creation_agent_owner_and_zero_attempt_evidence() {
    let (f, service) = Fixture::with_server(0, "idle");
    let root = prepared(&f);
    let observer = f.root().join("recovery-observer");
    let journal = util::read_state_json(&root, "upstream-session.json").unwrap();
    let runtime = util::read_state_json(&root, "runtime.json").unwrap();
    let agent = util::read_state_json(&observer, "agent.json").unwrap();
    let owner = util::read_state_json(&observer, "lifecycle-owner.json").unwrap();
    let context = util::read_state_json(&observer, "observer.json").unwrap();
    let env = f.active();
    let check = || crate::task_runtime::tui::human_snapshot(&env, &root, &f.root());
    assert!(check().is_ok());
    assert!(!root.join("presentation.lock").exists());
    for phase in [J::Null, json!("requested"), json!("unknown")] {
        let mut changed = journal.clone();
        if phase.is_null() {
            changed
                .as_object_mut()
                .unwrap()
                .remove("presentation_intent");
        } else {
            changed["presentation_intent"]["phase"] = phase;
        }
        util::atomic_write_json(&root.join("upstream-session.json"), &changed).unwrap();
        assert!(
            check().is_err(),
            "missing/requested/unknown intent must block"
        );
    }
    for (key, value) in [
        ("phase", json!("creating")),
        ("instance", json!("other")),
        ("session", json!(SESSION)),
        ("runtime", json!({})),
    ] {
        let mut changed = journal.clone();
        changed[key] = value;
        util::atomic_write_json(&root.join("upstream-session.json"), &changed).unwrap();
        assert!(check().is_err(), "changed creation {key} must block");
    }
    util::atomic_write_json(&root.join("upstream-session.json"), &journal).unwrap();
    for (key, value) in [
        ("phase", json!("creating")),
        ("runtime", json!(f.root())),
        ("session", json!(SESSION)),
        ("model", json!({})),
    ] {
        let mut changed = agent.clone();
        changed[key] = value;
        util::atomic_write_json(&observer.join("agent.json"), &changed).unwrap();
        assert!(check().is_err(), "changed agent {key} must block");
    }
    util::atomic_write_json(&observer.join("agent.json"), &agent).unwrap();
    util::atomic_write_json(&observer.join("lifecycle-owner.json"), &json!({})).unwrap();
    assert!(check().is_err());
    util::atomic_write_json(&observer.join("lifecycle-owner.json"), &owner).unwrap();
    for key in ["owner", "coordinator", "observer", "runtime"] {
        let mut changed = context.clone();
        changed[key] = J::Null;
        util::atomic_write_json(&observer.join("observer.json"), &changed).unwrap();
        assert!(check().is_err(), "changed context {key} must block");
    }
    util::atomic_write_json(&observer.join("observer.json"), &context).unwrap();
    for name in [
        "presentation.lock",
        "presentation.json",
        "tui-orphan",
        "broker-turn.json",
        "turn.json",
        "receiver-start.json",
        "native-client.json",
        "native-requests.json",
    ] {
        std::fs::write(root.join(name), "{}").unwrap();
        assert!(check().is_err(), "retained {name} must block");
        std::fs::remove_file(root.join(name)).unwrap();
    }
    util::atomic_write_json(&observer.join("incident.json"), &json!({})).unwrap();
    assert!(check().is_err());
    std::fs::remove_dir_all(observer.join("incident.json")).unwrap();
    let mut ordinary = runtime.clone();
    ordinary.as_object_mut().unwrap().remove("recovery_role");
    util::atomic_write_json(&root.join("runtime.json"), &ordinary).unwrap();
    assert!(
        check().is_err(),
        "ordinary configured runtime is not a lazy Recovery"
    );
    util::atomic_write_json(&root.join("runtime.json"), &runtime).unwrap();
    assert!(check().is_ok());
    assert!(!root.join("presentation.lock").exists());
    service.finish();
}
