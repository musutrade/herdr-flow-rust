//! Instance admission and crash checkpoints over isolated Git/queue fixtures.
use herdr_flow::{guard, lifecycle, model::J, progress, queue, task_env, task_runtime, util};
use serde_json::json;
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
    sync::{Mutex, MutexGuard},
};
use tempfile::TempDir;

static LOCK: Mutex<()> = Mutex::new(());
#[path = "common/isolated_review.rs"]
mod isolated_review;
#[path = "common/isolated_runtime.rs"]
mod isolated_runtime;
const SESSION: &str = "11111111-1111-1111-1111-111111111111";

struct Fixture {
    temp: TempDir,
    queue: PathBuf,
    commit: String,
    old_root: Option<std::ffi::OsString>,
    _lock: MutexGuard<'static, ()>,
}

impl Fixture {
    fn new() -> Self {
        let lock = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::Builder::new()
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        let old_root = std::env::var_os("HERDR_TASK_ENV_ROOT");
        std::env::set_var(
            "HERDR_TASK_ENV_ROOT",
            temp.path().join("queue/queue.json/task-environments"),
        );
        let repo = temp.path().join("repo");
        fs::create_dir(&repo).unwrap();
        fs::set_permissions(&repo, fs::Permissions::from_mode(0o700)).unwrap();
        git(&repo, &["init", "-q"]);
        git(
            &repo,
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.test",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "fixture",
            ],
        );
        let commit = git(&repo, &["rev-parse", "HEAD"]);
        let queue = temp.path().join("queue");
        queue::init_isolated(&queue, None, 30).unwrap();
        let receiver_dir = temp.path().join("receiver");
        fs::create_dir(&receiver_dir).unwrap();
        fs::set_permissions(&receiver_dir, fs::Permissions::from_mode(0o700)).unwrap();
        let receiver = json!({"version":2,"task":"fixture","role":"coordinator","pane":"w1:p1",
            "terminal":"term1","socket":"/fixture","cwd":repo,"session":SESSION,"phase":"monitoring"});
        util::atomic_write_json(&receiver_dir.join("state.json"), &receiver).unwrap();
        let mut s = queue::load_queue(&queue).unwrap();
        s["guard_dir"] = json!(receiver_dir);
        s["receiver"] = guard::scope(&receiver);
        s["tasks"] = json!({"task":{"task":"task","epoch":1,"state_version":1,"phase":"waiting",
            "candidate":commit,"control":"running","writer":null,"pending":[],"operations":{}}});
        queue::save(&queue, &mut s).unwrap();
        Self {
            temp,
            queue,
            commit,
            old_root,
            _lock: lock,
        }
    }

    fn state(&self) -> J {
        queue::load_queue(&self.queue).unwrap()
    }

    fn create_spec(&self) -> J {
        let s = self.state();
        json!({"action":"create","queue_id":s["id"],"task":"task","epoch":1,
            "state_version":s["tasks"]["task"]["state_version"],"environment":{
                "workspace_root":self.temp.path().join("workspace"),"temp_root":self.temp.path().join("scratch"),
                "delivery_targets":[],"repository":self.temp.path().join("repo"),"commit":self.commit}})
    }

    fn call(&self, spec: &J) -> herdr_flow::Result<J> {
        let _lock = queue::locked(&self.queue, false)?;
        let factory = |_: &J| -> herdr_flow::Result<Box<dyn herdr_flow::herdr::ReceiverAdapter>> {
            panic!("environment admission must not contact Herdr")
        };
        queue::task_action(
            &self.queue,
            "task-environment",
            spec,
            Some(SESSION),
            &factory,
        )
    }

    fn control_ready(&self) {
        let mut s = self.state();
        s["tasks"]["bootstrap-helper"] = json!({"task":"bootstrap-helper","epoch":1,"state_version":1,
            "candidate":self.commit,"phase":"waiting","control":"running","writer":null,"pending":[],"operations":{}});
        {
            let _queue = queue::locked(&self.queue, false).unwrap();
            queue::save(&self.queue, &mut s).unwrap();
        }
        self.call(&json!({"action":"create","task":"bootstrap-helper","queue_id":s["id"],"epoch":1,"state_version":1,
            "environment":{"workspace_root":self.temp.path().join("bootstrap-work"),"temp_root":self.temp.path().join("bootstrap-temp"),
            "delivery_targets":[],"repository":self.temp.path().join("repo"),"commit":self.commit}})).unwrap();
        let s = self.state();
        let owner = lifecycle::task_owner(&s, "bootstrap-helper")
            .unwrap()
            .unwrap();
        queue::finalization::task_action(&self.queue, "task-finalize", &json!({"task":owner.task_id,"queue_id":owner.queue_id,
            "epoch":owner.epoch,"instance_id":owner.instance_id,"state_version":s["tasks"]["bootstrap-helper"]["state_version"],
            "outcome":"failure","reason":"bootstrap fixture complete"}), Some(SESSION), &|_| panic!("no model")).unwrap();
        assert!(self.state()["tasks"]["bootstrap-helper"].is_null());
        assert!(!self.temp.path().join("bootstrap-work").exists());
        assert!(!self.temp.path().join("bootstrap-temp").exists());
        assert!(!task_env::is_registered("bootstrap-helper").unwrap());
        assert!(!task_env::environment_state_path("bootstrap-helper")
            .unwrap()
            .exists());
        assert!(!task_env::cleanup_state_path("bootstrap-helper")
            .unwrap()
            .exists());
        assert!(!queue::artifacts(&self.queue)
            .unwrap()
            .join("gates/bootstrap-helper.lock")
            .exists());
        assert!(!git(
            &self.temp.path().join("repo"),
            &["worktree", "list", "--porcelain"]
        )
        .contains("bootstrap-work/checkout"));
    }

    fn seed(&self) -> task_env::TaskEnvironment {
        self.control_ready();
        let mut s = self.state();
        let binding = task_env::instance::InstanceBinding {
            queue_id: s["id"].as_str().unwrap().into(),
            task_id: "task".into(),
            epoch: 1,
        };
        let owner = lifecycle::Owner {
            queue_root: self.queue.clone(),
            queue_id: binding.queue_id.clone(),
            task_id: "task".into(),
            epoch: 1,
            instance_id: String::new(),
        };
        let _queue = queue::locked(&self.queue, false).unwrap();
        let lease = std::sync::Arc::new(lifecycle::acquire(&owner, true).unwrap());
        let env = task_env::instance::prepare_isolated(
            &binding,
            &serde_json::from_value(self.create_spec()["environment"].clone()).unwrap(),
            &self.queue,
            lease,
        )
        .unwrap();
        assert!(!env.state_file().exists());
        progress::environment::checkpoint(&mut s, &binding, &env, "creating", util::now()).unwrap();
        queue::save(&self.queue, &mut s).unwrap();
        env
    }

    fn initialize_seed(&self, env: &mut task_env::TaskEnvironment) {
        let _queue = queue::locked(&self.queue, false).unwrap();
        task_env::instance::initialize_prepared(env).unwrap();
        let binding = env.creation.as_ref().unwrap().binding.clone();
        let mut s = self.state();
        progress::environment::checkpoint(&mut s, &binding, env, "creating", util::now()).unwrap();
        queue::save(&self.queue, &mut s).unwrap();
        assert!(!s["tasks"]["task"]["environment"]["bootstrap"].is_null());
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        match &self.old_root {
            Some(value) => std::env::set_var("HERDR_TASK_ENV_ROOT", value),
            None => std::env::remove_var("HERDR_TASK_ENV_ROOT"),
        }
    }
}

fn git(repo: &Path, args: &[&str]) -> String {
    use std::os::unix::process::CommandExt;
    let mut command = Command::new("git");
    command.arg("-C").arg(repo).args(args);
    // SAFETY: umask takes a scalar and only changes this fork child's mask.
    unsafe {
        command.pre_exec(|| {
            libc::umask(0o077);
            Ok(())
        });
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

impl Fixture {
    fn ready(&self) -> task_env::TaskEnvironment {
        self.call(&self.create_spec()).unwrap();
        task_env::restore_environment("task").unwrap()
    }
    fn finalize(&self, outcome: &str) -> herdr_flow::Result<J> {
        let s = self.state();
        let t = &s["tasks"]["task"];
        let spec = json!({"task":"task","queue_id":s["id"],"epoch":1,"instance_id":t["environment"]["instance_id"],
            "state_version":t["state_version"],"outcome":outcome,"reason":"explicit fixture terminal"});
        let _lock = queue::locked(&self.queue, false)?;
        queue::task_action(
            &self.queue,
            "task-finalize",
            &spec,
            Some(SESSION),
            &|_: &J| -> herdr_flow::Result<Box<dyn herdr_flow::herdr::ReceiverAdapter>> {
                panic!("no model")
            },
        )
    }
}

#[test]
fn isolated_authorities_reject_legacy_file_writes_and_sidecars() {
    let f = Fixture::new();
    let env = f.ready();
    for path in [
        f.queue.join("queue.json"),
        task_env::registry_path().unwrap(),
        env.state_file().to_path_buf(),
    ] {
        assert!(path.is_dir());
        let original = util::read_json(&path).unwrap();
        assert!(fs::read_to_string(&path).is_err());
        assert!(util::atomic_write_bytes(&path, b"{}").is_err());
        assert!(fs::remove_file(&path).is_err());
        assert_eq!(util::read_json(&path).unwrap(), original);
    }
    let artifacts = queue::artifacts(&f.queue).unwrap();
    let compact = artifacts.join("compact.json");
    util::atomic_write_json_compact(&compact, &json!({"protected":true})).unwrap();
    assert!(compact.is_dir());
    assert_eq!(
        util::read_json(&compact).unwrap(),
        json!({"protected":true})
    );
    assert_eq!(
        util::read_json(&compact.join("authority.json")).unwrap(),
        json!({"protected":true})
    );
    let namespace = artifacts.join("test-results");
    util::protected::namespace(&namespace).unwrap();
    let fake_payload = namespace.join("authority.json");
    fs::write(&fake_payload, b"{}").unwrap();
    assert!(util::read_json(&fake_payload).is_err());
    assert!(util::read_state_json(&namespace, "authority.json").is_err());
    assert!(util::atomic_write_json(&fake_payload, &json!({"replacement":true})).is_err());
    assert_eq!(fs::read(&fake_payload).unwrap(), b"{}");
    let foreign = artifacts.join("foreign.json");
    fs::write(&foreign, b"{}").unwrap();
    assert!(util::read_state_json(&artifacts, "foreign.json").is_err());
    assert!(util::atomic_write_json(&foreign, &json!({})).is_err());
    assert_eq!(task_env::status("task").unwrap()["state"], "active");
}

#[test]
fn action_permit_excludes_terminal_then_terminal_blocks_late_actions() {
    let f = Fixture::new();
    let env = f.ready();
    let owner = task_env::isolation::owner(&env).unwrap();
    drop(env);
    let permit = lifecycle::write_permit(&owner).unwrap();
    assert!(lifecycle::acquire(&owner, true).is_err());
    assert!(f.finalize("failure").is_err());
    drop(permit);
    let s = f.finalize("failure").unwrap();
    assert_eq!(s["tasks"]["task"]["final_outcome"], "failure");
    assert_eq!(
        s["tasks"]["task"]["cleanup_obligation"]["status"],
        "pending"
    );
    assert!(f.temp.path().join("workspace/checkout").is_dir());
    assert!(lifecycle::write_permit(&owner).is_err());
    assert!(f.finalize("success").is_err());
    assert_eq!(f.finalize("failure").unwrap()["tasks"], s["tasks"]);
    let permit = lifecycle::acquire(&owner, true).unwrap();
    let env = task_env::restore_for_terminal(&owner, &permit).unwrap();
    assert!(env.persist().is_err());
}

#[test]
fn raw_commands_never_authorize_delayed_isolated_spawn() {
    let f = Fixture::new();
    let env = f.ready();
    let mut delayed = env.command("/bin/true");
    assert!(delayed.spawn().is_err());
    let root = task_runtime::prepare(&env, "fixture").unwrap();
    let owner = task_env::isolation::owner(&env).unwrap();
    assert_eq!(
        util::read_state_json(&root, "lifecycle-owner.json").unwrap(),
        serde_json::to_value(&owner).unwrap()
    );
    let permit = lifecycle::runtime_permit(&root).unwrap().unwrap();
    assert!(lifecycle::acquire(&owner, true).is_err());
    drop(permit);
    // No runtime/process-lifetime permit remains after a bounded call.
    drop(lifecycle::acquire(&owner, true).unwrap());
    drop(env);
    f.finalize("failure").unwrap();
    assert!(lifecycle::runtime_permit(&root).is_err());
    assert!(guard::advise(&root, &root.join("advice.json"), SESSION).is_err());
    assert!(delayed.spawn().is_err());
}

#[test]
fn owner_removal_and_registry_backdoors_cannot_release_isolated_claims() {
    let f = Fixture::new();
    let mut env = f.ready();
    assert!(task_env::unregister_environment("task", &env.instance_id).is_err());
    let entry = task_env::load_registry()
        .unwrap()
        .get("task")
        .unwrap()
        .clone();
    assert!(task_env::register_environment(entry).is_err());
    assert!(task_env::claim_roots_exclusively("task", &env.instance_id, &[]).is_err());
    let before = util::read_json(&task_env::registry_path().unwrap()).unwrap();
    assert!(task_env::cleanup_failed_environment("task").is_err());
    assert!(task_env::resume_delivery_cleanup("task").is_err());
    assert!(task_env::recover_interrupted("task").is_err());
    assert!(task_env::cleanup_failed_environment("absent").is_err());
    assert!(task_env::recover_interrupted("absent").is_err());
    assert_eq!(
        util::read_json(&task_env::registry_path().unwrap()).unwrap(),
        before
    );
    assert!(env.workspace_root.join("checkout").is_dir());
    env.creation = None;
    assert!(env.command("/bin/true").spawn().is_err());
    assert!(env.persist().is_err());
    assert!(task_env::destroy_environment(&env).is_err());
    assert!(env.workspace_root.join("checkout").is_dir());
    assert!(task_env::load_registry().unwrap().contains("task"));
}

#[test]
fn missing_authority_is_not_recreated_by_acquire_or_stale_handle() {
    let f = Fixture::new();
    let env = f.ready();
    let owner = task_env::isolation::owner(&env).unwrap();
    let old = f.temp.path().join("preserved-authority");
    fs::rename(f.queue.join("queue.json"), &old).unwrap();
    assert!(lifecycle::write_permit(&owner).is_err());
    assert!(env.persist().is_err());
    assert!(!f.queue.join("queue.json").exists());
    fs::rename(&old, f.queue.join("queue.json")).unwrap();
    let original = env.state_file().to_path_buf();
    fs::rename(&original, f.temp.path().join("preserved-env")).unwrap();
    assert!(env.persist().is_err());
    assert!(!original.exists());
}

#[test]
fn failed_creation_can_record_terminal_without_adopting_unknown_directory() {
    let f = Fixture::new();
    let mut env = f.seed();
    f.initialize_seed(&mut env);
    fs::create_dir(&env.workspace_root).unwrap();
    assert!(task_env::instance::materialize_instance(&mut env).is_err());
    drop(env);
    f.finalize("failure").unwrap();
    assert!(f.temp.path().join("workspace").is_dir());
}

struct FixtureChild(std::process::Child);
impl Drop for FixtureChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn idle_broker_does_not_hold_gate_and_late_rpc_never_contacts_native_server() {
    use std::{
        io::Write,
        os::unix::net::{UnixListener, UnixStream},
        time::{Duration, Instant},
    };
    let f = Fixture::new();
    let env = f.ready();
    let root = task_runtime::prepare(&env, "broker").unwrap();
    let owner = task_env::isolation::owner(&env).unwrap();
    util::atomic_write_json(
        &root.join("runtime.json"),
        &json!({"instance":env.instance_id,"lifecycle_owner":owner}),
    )
    .unwrap();
    let server = UnixListener::bind(root.join("server.sock")).unwrap();
    server.set_nonblocking(true).unwrap();
    let mut child = FixtureChild(
        Command::new(env!("CARGO_BIN_EXE_herdr-flow"))
            .args(["task-runtime-broker", "--state-dir"])
            .arg(&root)
            .env(task_env::MARKER_ENV, &env.instance_id)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "fixture broker exited before readiness"
        );
        if util::read_state_json(&root, "broker-status.json")
            .is_ok_and(|s| s["pid"] == child.0.id() && s["phase"] == "running")
        {
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    let before = util::read_state_json(&root, "broker-status.json").unwrap();
    drop(env);
    // Readiness persistence precedes release of the bounded startup permit.
    // Busy is an explicit no-mutation retry, not a permanent broker lease.
    let before_task = f.state()["tasks"].clone();
    loop {
        match f.finalize("failure") {
            Ok(_) => break,
            Err(e) if e.message.contains("lifecycle action busy") => {
                assert_eq!(f.state()["tasks"], before_task);
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(e) => panic!("{e}"),
        }
    }
    let mut socket = UnixStream::connect(root.join("broker.sock")).unwrap();
    writeln!(socket, "{}", json!({"method":"turn/start","params":{}})).unwrap();
    drop(socket);
    loop {
        if let Some(exit) = child.0.try_wait().unwrap() {
            assert!(!exit.success());
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(matches!(server.accept(),Err(e) if e.kind()==std::io::ErrorKind::WouldBlock));
    assert_eq!(
        util::read_state_json(&root, "broker-status.json").unwrap(),
        before
    );
    assert!(!root.join("broker-events.json").exists());
}

#[test]
fn independent_ci_watcher_cannot_be_mistaken_for_managed_observer() {
    let f = Fixture::new();
    let env = f.ready();
    let root = task_runtime::prepare(&env, "observer").unwrap();
    assert!(herdr_flow::ci::watch::persist(&root, "hash", &json!({}))
        .unwrap_err()
        .message
        .contains("unsupported"));
    assert!(
        herdr_flow::ci::watch::run(Path::new("/not-read"), &root, true, None)
            .unwrap_err()
            .message
            .contains("unsupported")
    );
    assert!(!root.join("status.json").exists());
    assert!(!root.join("identity.json").exists());
    let output = Command::new(env!("CARGO_BIN_EXE_herdr-flow"))
        .args(["ci", "--config", "/not-read", "--state-dir"])
        .arg(f.temp.path().join("unregistered-observer"))
        .env(
            "HERDR_LIFECYCLE_OWNER",
            serde_json::to_string(&task_env::isolation::owner(&env)).unwrap(),
        )
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unsupported"));
    assert!(!f.temp.path().join("unregistered-observer").exists());
}

#[test]
fn isolated_resume_recovers_exact_seed_before_envdoc_and_ready() {
    let f = Fixture::new();
    let env = f.seed();
    let binding = env.creation.as_ref().unwrap().binding.clone();
    let id = env.instance_id.clone();
    assert!(!env.state_file().exists());
    assert_eq!(
        f.state()["tasks"]["task"]["environment"]["creation_seed"],
        json!(env)
    );
    assert!(f.state()["tasks"]["task"]["environment"]["bootstrap"].is_null());
    drop(env);
    let error = f.call(&f.create_spec()).unwrap_err();
    assert!(
        error.to_string().contains("environment already bound"),
        "{error}"
    );
    let resume = json!({"action":"resume","task":"task","queue_id":binding.queue_id,"epoch":1,
        "instance_id":id,"state_version":f.state()["tasks"]["task"]["state_version"]});
    let active = f.call(&resume).unwrap();
    assert_eq!(active["tasks"]["task"]["environment"]["phase"], "active");
    let mut retry = resume;
    retry["state_version"] = active["tasks"]["task"]["state_version"].clone();
    let ready = f.call(&retry).unwrap();
    assert_eq!(ready["tasks"]["task"]["environment"]["instance_id"], id);
    let owner = lifecycle::task_owner(&ready, "task").unwrap().unwrap();
    let workspace = f.temp.path().join("workspace");
    fs::write(workspace.join("checkout/untracked"), "task output").unwrap();
    let receiver = fs::read(f.temp.path().join("receiver/state.json")).unwrap();
    // Continue the recovered instance through the real terminal driver, rather
    // than relying on TempDir to remove the resumed worktree and registration.
    f.finalize("failure").unwrap();
    let report = queue::finalization::drive(&f.queue).unwrap();
    assert_eq!(report.completed, 1, "{report:?}");
    assert!(!workspace.exists());
    assert!(!f.temp.path().join("scratch").exists());
    assert!(!task_env::is_registered("task").unwrap());
    assert!(!task_env::environment_state_path("task").unwrap().exists());
    assert!(!task_env::cleanup_state_path("task").unwrap().exists());
    assert!(f.state()["tasks"]["task"].is_null());
    assert!(!git(
        &f.temp.path().join("repo"),
        &["worktree", "list", "--porcelain"]
    )
    .contains("workspace/checkout"));
    assert!(lifecycle::write_permit(&owner).is_err());
    assert_eq!(
        fs::read(f.temp.path().join("receiver/state.json")).unwrap(),
        receiver
    );
    assert_eq!(queue::finalization::drive(&f.queue).unwrap().completed, 0);
}

#[test]
fn managed_fixture_spawn_registers_under_permit_and_closing_refuses_new_spawn() {
    let f = Fixture::new();
    let mut env = f.ready();
    let root = task_runtime::prepare(&env, "spawn").unwrap();
    let late = task_runtime::prepare(&env, "late").unwrap();
    let binary = root.join("fixture.sh");
    fs::write(&binary, b"#!/bin/sh\nexec sleep 30\n").unwrap();
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
    // This synthetic executable ignores server arguments; no model or HTTP server.
    let spec = json!({"adapter":"opencode","configuration":"isolated-fixture","binary":binary,
        "cwd":env.workspace_root.join("checkout"),"port":1,
        "model":{"providerID":"fixture","id":"no-model"}});
    let mut child = FixtureChild(task_runtime::launch(&mut env, &root, &spec).unwrap());
    let owner = task_env::isolation::owner(&env).unwrap();
    let record = util::read_state_json(&root, "runtime.json").unwrap();
    assert_eq!(
        record["lifecycle_owner"],
        serde_json::to_value(&owner).unwrap()
    );
    assert_eq!(record["pid"], child.0.id());
    assert!(!env.child_processes.is_empty());
    drop(lifecycle::acquire(&owner, true).unwrap());
    assert!(child.0.try_wait().unwrap().is_none());
    drop(env);
    f.finalize("failure").unwrap();
    // PR2 records the obligation while the registered fixture process is alive.
    // It deliberately does not perform the future PR3 stop stage.
    assert!(child.0.try_wait().unwrap().is_none());
    child.0.kill().unwrap();
    child.0.wait().unwrap();
    let mut env = task_env::restore_environment("task").unwrap();
    assert!(task_runtime::launch(&mut env, &late, &spec).is_err());
    assert!(!late.join("runtime.json").exists());
    assert_eq!(
        util::read_state_json(&root, "runtime.json").unwrap(),
        record
    );
}

#[test]
fn missing_runtime_namespace_or_owner_never_downgrades_to_legacy_writes() {
    let f = Fixture::new();
    let env = f.ready();
    let root = task_runtime::prepare(&env, "namespace").unwrap();
    assert_eq!(task_runtime::prepare(&env, "namespace").unwrap(), root);
    let queue_before = f.state();
    let owner = root.join("lifecycle-owner.json");
    let owner_before = util::read_json(&owner).unwrap();
    let marker = root.join(".flow-namespace-v2");
    fs::rename(&marker, root.join("held-namespace")).unwrap();
    assert!(lifecycle::runtime_permit(&root).is_err());
    assert!(task_runtime::prepare(&env, "namespace").is_err());
    assert!(guard::advise(&root, &root.join("new-advice.json"), SESSION).is_err());
    assert!(!root.join("new-advice.json").exists());
    assert_eq!(util::read_json(&owner).unwrap(), owner_before);
    fs::rename(&owner, root.join("held-owner")).unwrap();
    assert!(lifecycle::runtime_permit(&root).is_err());
    assert!(util::protected::is_runtime_namespace(&root).is_err());
    assert!(!owner.exists());
    assert_eq!(f.state(), queue_before);
    fs::rename(root.join("held-namespace"), &marker).unwrap();
    fs::rename(root.join("held-owner"), &owner).unwrap();
    let mut other = owner_before.clone();
    other["instance_id"] = json!("foreign");
    util::atomic_write_json(&owner, &other).unwrap();
    assert!(task_runtime::prepare(&env, "namespace").is_err());
    assert_eq!(util::read_json(&owner).unwrap(), other);
}

#[test]
fn registry_missing_namespace_keeps_claims_and_refuses_recovery_or_creation() {
    let f = Fixture::new();
    let env = f.ready();
    let registry = task_env::registry_path().unwrap();
    let before = util::read_json(&registry).unwrap();
    fs::rename(
        registry.join(".flow-namespace-v2"),
        registry.join("held-namespace"),
    )
    .unwrap();
    assert!(task_env::unregister_environment("task", &env.instance_id).is_err());
    assert!(task_env::recover_interrupted("task").is_err());
    assert!(task_env::destroy_environment(&env).is_err());
    assert!(task_env::environment_state_path("new-task").is_err());
    assert_eq!(util::read_json(&registry).unwrap(), before);
    assert!(env.workspace_root.join("checkout").is_dir());
}
