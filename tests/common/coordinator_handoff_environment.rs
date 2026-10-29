//! Exercise the new operator entry against the real isolated environment and
//! protected queue. The Unix listener is deliberately never asked to run work.
use super::*;
use crate::task_runtime::coordinator_handoff as handoff;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::process::Stdio;

#[test]
#[ignore = "bounded-lifetime child of the executable identity fixture"]
fn frozen_driver_process() {
    use std::io::Read;
    let ready = std::env::var_os("HERDR_HANDOFF_TEST_CHILD").expect("fixture child only");
    std::fs::write(ready, b"ready").unwrap();
    std::io::stdin().read_to_end(&mut Vec::new()).unwrap();
}

fn frozen_driver(f: &Fixture) -> Child {
    let ready = f.temp.path().join("frozen-driver-ready");
    let child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "task_runtime::human::tests::coordinator_handoff::frozen_driver_process",
            "--ignored",
            "--nocapture",
        ])
        .env("HERDR_HANDOFF_TEST_CHILD", &ready)
        .env(task_env::MARKER_ENV, &f.owner.instance_id)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .process_group(0)
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !ready.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "frozen test process did not start"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    child
}

fn prepare(f: &mut Fixture) -> UnixListener {
    f.broker.kill().unwrap();
    f.broker.wait().unwrap();
    f.broker = frozen_driver(f);
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
    runtime["driver_binary"] = json!(std::env::current_exe().unwrap());
    runtime["role_models"] = json!({"developer":model,"reviewer":model,"recovery":model});
    util::atomic_write_json(&root.join("runtime.json"), &runtime).unwrap();
    let observer = root.join("recovery-observer");
    util::atomic_write_json(
        &observer.join("observer.json"),
        &json!({"owner":f.owner,"coordinator":root,"observer":observer,
            "runtime_parent":root.parent(),"runtime":runtime}),
    )
    .unwrap();
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

#[test]
fn interrupted_cancellation_keeps_the_fence_until_the_same_request_repairs_its_mirror() {
    let mut f = Fixture::new();
    let _listener = prepare(&mut f);
    request(&f, "replacement-1").unwrap();
    let original = util::read_state_json(&f.root(), "receiver-handoff.json").unwrap();
    assert_eq!(original["phase"], "draining");
    {
        let _exclusive = f.active();
        let mut state = f.state();
        let record = &mut state["tasks"]["parent"][handoff::KEY];
        record["records"][0]["phase"] = json!("cancelled");
        record["active_generation"] = J::Null;
        f.write_state(&mut state);
    }
    let committed = f.state();
    assert!(!automatic_input_allowed(&f.root()).unwrap());
    assert!(check_workspace(&committed, &f.checkout()).is_err());
    check_workspace(&committed, Path::new("/independent/checkout")).unwrap();
    handoff::admit_control(&committed, &f.owner, "inspect").unwrap();
    assert!(handoff::admit_control(&committed, &f.owner, "run").is_err());
    cancel(&f, "replacement-1").unwrap();
    assert_eq!(f.state(), committed);
    assert!(automatic_input_allowed(&f.root()).unwrap());
    let aligned = util::read_state_json(&f.root(), "receiver-handoff.json").unwrap();
    assert_eq!(aligned["phase"], "cancelled");
    assert_ne!(aligned["history_sha256"], original["history_sha256"]);
    util::protected::remove_document(&f.root().join("receiver-handoff.json")).unwrap();
    assert!(!automatic_input_allowed(&f.root()).unwrap());
    request(&f, "replacement-1").unwrap();
    assert_eq!(
        util::read_state_json(&f.root(), "receiver-handoff.json").unwrap(),
        aligned
    );
    assert!(automatic_input_allowed(&f.root()).unwrap());
}

#[test]
fn foreign_or_unknown_mirrors_are_preserved_without_adopting_or_replaying() {
    let mut f = Fixture::new();
    let _listener = prepare(&mut f);
    let mirror_path = f.root().join("receiver-handoff.json");
    util::atomic_write_json(&mirror_path, &json!({"foreign":true})).unwrap();
    let original = f.state();
    assert!(!automatic_input_allowed(&f.root()).unwrap());
    assert!(check_workspace(&original, &f.checkout()).is_err());
    assert!(handoff::admit_control(&original, &f.owner, "run").is_err());
    handoff::admit_control(&original, &f.owner, "inspect").unwrap();
    assert!(request(&f, "replacement-1").is_err());
    assert_eq!(f.state(), original);
    util::protected::remove_document(&mirror_path).unwrap();
    request(&f, "replacement-1").unwrap();
    cancel(&f, "replacement-1").unwrap();
    let original = f.state();
    let aligned = util::read_state_json(&f.root(), "receiver-handoff.json").unwrap();
    for (field, value) in [
        ("owner", json!({"instance_id":"foreign"})),
        ("history_sha256", json!("0".repeat(64))),
        ("phase", json!("complete")),
    ] {
        let mut invalid = aligned.clone();
        invalid[field] = value;
        util::atomic_write_json(&mirror_path, &invalid).unwrap();
        assert!(!automatic_input_allowed(&f.root()).unwrap());
        assert!(cancel(&f, "replacement-1").is_err());
        assert_eq!(f.state(), original);
        assert_eq!(
            util::read_state_json(&f.root(), "receiver-handoff.json").unwrap(),
            invalid
        );
    }
}

#[test]
fn unconfigured_recovery_remains_absent_and_cannot_adopt_an_old_observer() {
    let mut f = Fixture::new();
    let listener = prepare(&mut f);
    let mut runtime = util::read_state_json(&f.root(), "runtime.json").unwrap();
    runtime["role_models"]
        .as_object_mut()
        .unwrap()
        .remove("recovery");
    util::atomic_write_json(&f.root().join("runtime.json"), &runtime).unwrap();
    let before = f.state();
    assert!(request(&f, "replacement-1").is_err());
    assert_eq!(f.state(), before);
    // This fixture owns only synthetic observer metadata, with no Recovery
    // process or model session. Remove it to model a never-configured role.
    std::fs::remove_dir_all(f.root().join("recovery-observer")).unwrap();
    let observer = f.root().join("recovery-observer");
    std::os::unix::fs::symlink(f.root().join("missing-observer"), &observer).unwrap();
    assert!(request(&f, "replacement-1").is_err());
    assert_eq!(f.state(), before);
    std::fs::remove_file(&observer).unwrap();
    let result = request(&f, "replacement-1").unwrap();
    assert!(result["records"][0]["recovery"].is_null());
    cancel(&f, "replacement-1").unwrap();
    assert!(!f.root().join("recovery-observer").exists());
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[test]
fn request_checks_the_running_executable_and_its_restart_path_before_fencing() {
    let mut f = Fixture::new();
    let _listener = prepare(&mut f);
    let before = f.state();
    let original = util::read_state_json(&f.root(), "runtime.json").unwrap();
    let mut runtime = original.clone();
    runtime["driver_binary"] = json!("/usr/bin/sleep");
    util::atomic_write_json(&f.root().join("runtime.json"), &runtime).unwrap();
    assert!(request(&f, "replacement-1")
        .unwrap_err()
        .message
        .contains("restart path"));
    // Claiming the new driver path cannot upgrade an already running old image.
    f.broker.kill().unwrap();
    f.broker.wait().unwrap();
    f.broker = Command::new("sleep")
        .arg("300")
        .env(task_env::MARKER_ENV, &f.owner.instance_id)
        .process_group(0)
        .spawn()
        .unwrap();
    runtime = original;
    runtime["pid"] = json!(f.broker.id());
    runtime["start_time"] = json!(task_env::process_start_time(f.broker.id() as i32).unwrap());
    util::atomic_write_json(&f.root().join("runtime.json"), &runtime).unwrap();
    assert!(request(&f, "replacement-1")
        .unwrap_err()
        .message
        .contains("actual running frozen executable"));
    assert_eq!(f.state(), before);
    assert!(!f.root().join("receiver-handoff.json").exists());
}

#[test]
fn each_generation_rechecks_original_recovery_configuration_and_protection() {
    let mut f = Fixture::new();
    let _listener = prepare(&mut f);
    request(&f, "replacement-1").unwrap();
    cancel(&f, "replacement-1").unwrap();
    let before = f.state();
    let observer = f.root().join("recovery-observer");
    let original = f.context();
    for (field, value) in [
        ("root", json!(f.root().join("other-runtime"))),
        ("lifecycle_owner", json!({"instance_id":"foreign"})),
        ("model", json!({"providerID":"openai","id":"different"})),
    ] {
        let mut context = original.clone();
        context["runtime"][field] = value;
        util::atomic_write_json(&observer.join("observer.json"), &context).unwrap();
        assert!(request(&f, "replacement-2").is_err(), "{field}");
        assert_eq!(f.state(), before);
    }
    util::atomic_write_json(&observer.join("observer.json"), &original).unwrap();
    let owner = util::read_state_json(&observer, "lifecycle-owner.json").unwrap();
    util::atomic_write_json(
        &observer.join("lifecycle-owner.json"),
        &json!({"foreign":true}),
    )
    .unwrap();
    assert!(request(&f, "replacement-2").is_err());
    assert_eq!(f.state(), before);
    util::atomic_write_json(&observer.join("lifecycle-owner.json"), &owner).unwrap();
    let saved = f.root().join("saved-observer");
    std::fs::rename(&observer, &saved).unwrap();
    std::fs::create_dir(&observer).unwrap();
    std::fs::write(
        observer.join("observer.json"),
        serde_json::to_vec(&original).unwrap(),
    )
    .unwrap();
    assert!(request(&f, "replacement-2").is_err());
    assert_eq!(f.state(), before);
    std::fs::remove_dir_all(&observer).unwrap();
    std::fs::rename(&saved, &observer).unwrap();
    request(&f, "replacement-2").unwrap();
}

#[test]
fn cancelled_current_mirror_never_authorizes_another_runtime_or_its_orphan_mirror() {
    let mut f = Fixture::new();
    let _listener = prepare(&mut f);
    request(&f, "replacement-1").unwrap();
    cancel(&f, "replacement-1").unwrap();
    let state = f.state();
    let other = f.root().parent().unwrap().join("older-runtime");
    let mut runtime = util::read_state_json(&f.root(), "runtime.json").unwrap();
    runtime["root"] = json!(other);
    {
        let env = f.active();
        task_env::isolation::runtime_namespace(&env, &other).unwrap();
        util::atomic_write_json(&other.join("runtime.json"), &runtime).unwrap();
    }
    assert!(!handoff::runtime_blocked(&other, &runtime).unwrap());
    check_workspace(&state, &f.checkout()).unwrap();
    let mirror = other.join("receiver-handoff.json");
    util::atomic_write_json(&mirror, &json!({"foreign":true})).unwrap();
    assert!(handoff::runtime_blocked(&other, &runtime).is_err());
    assert!(check_workspace(&state, &f.checkout()).is_err());
    check_workspace(&state, Path::new("/independent/checkout")).unwrap();
    util::protected::remove_document(&mirror).unwrap();
    std::os::unix::fs::symlink(other.join("missing"), &mirror).unwrap();
    assert!(handoff::runtime_blocked(&other, &runtime).is_err());
    assert!(check_workspace(&state, &f.checkout()).is_err());
    assert_eq!(f.state(), state);
}

#[test]
fn an_old_watcher_or_omitted_marked_process_prevents_installing_the_fence() {
    struct Extra(Child);
    impl Drop for Extra {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut f = Fixture::new();
    let _listener = prepare(&mut f);
    let before = f.state();
    let mut old = Extra(
        Command::new("/usr/bin/sleep")
            .arg("60")
            .env(task_env::MARKER_ENV, &f.owner.instance_id)
            .spawn()
            .unwrap(),
    );
    let pid = old.0.id() as i32;
    let watch = f.root().join("guard/watch.json");
    util::atomic_write_json(
        &watch,
        &json!({"pid":pid,
        "start_ticks":task_env::process_start_time(pid).unwrap().to_string()}),
    )
    .unwrap();
    assert!(request(&f, "replacement-1")
        .unwrap_err()
        .message
        .contains("actual running frozen executable"));
    util::protected::remove_document(&watch).unwrap();
    // Omitting its watcher record is not proof that the old task process died.
    assert!(request(&f, "replacement-1").is_err());
    assert_eq!(f.state(), before);
    assert!(!f.root().join("receiver-handoff.json").exists());
    old.0.kill().unwrap();
    old.0.wait().unwrap();
    request(&f, "replacement-1").unwrap();
}
