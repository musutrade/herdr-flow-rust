//! In-process coverage of the real watcher tick and its isolated Owner gate.
//! A local broker protocol peer answers queries; no native model is started.
use super::*;
use crate::{lifecycle, queue, task_env};
use std::{
    collections::BTreeMap,
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::net::{UnixListener, UnixStream},
    time::{Duration, Instant},
};

const SESSION: &str = "11111111-1111-1111-1111-111111111111";

struct Fixture {
    env: TaskEnvironment,
    queue: PathBuf,
    guard: PathBuf,
    state: J,
    listener: UnixListener,
    _registry: task_env::TestEnvRootGuard,
    directory: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let directory = util::fs::test_tempdir();
        let queue = directory.path().join("queue");
        let registry = task_env::test_env_root(&queue.join("queue.json/task-environments"));
        let repository = directory.path().join("repository");
        util::ensure_private_dir(&repository, true).unwrap();
        git(&repository, &["init", "--quiet"]);
        git(
            &repository,
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--allow-empty",
                "-qm",
                "receiver baseline",
            ],
        );
        let commit = git(&repository, &["rev-parse", "HEAD"]);
        queue::init_isolated(&queue, None, 30).unwrap();
        let registration = directory.path().join("coordinator");
        util::ensure_private_dir(&registration, true).unwrap();
        let coordinator = json!({"version":2,"task":"fixture","role":"coordinator","pane":"window:pane","terminal":"terminal","socket":"/fixture","cwd":repository,"session":SESSION,"phase":"monitoring"});
        util::atomic_write_json(&registration.join("state.json"), &coordinator).unwrap();
        let mut state = queue::load_queue(&queue).unwrap();
        state["guard_dir"] = json!(registration);
        state["receiver"] = crate::guard::scope(&coordinator);
        state["tasks"] = json!({"task":{"task":"task","epoch":1,"state_version":1,"candidate":commit,"control":"running","phase":"waiting","writer":null,"pending":[],"operations":{}}});
        queue::save(&queue, &mut state).unwrap();
        action(
            &queue,
            "task-environment",
            &json!({"action":"create","task":"task","state_version":1,"queue_id":state["id"],"epoch":1,
            "environment":{"workspace_root":directory.path().join("workspace"),"temp_root":directory.path().join("scratch"),"delivery_targets":[],"repository":repository,"commit":commit}}),
        );
        let env = task_env::restore_environment("task").unwrap();
        let owner = task_env::isolation::owner(&env).unwrap();
        let root = crate::task_runtime::prepare(&env, "receiver-tick").unwrap();
        let guard = root.join("guard");
        task_env::isolation::runtime_namespace(&env, &guard).unwrap();
        let pid = std::process::id();
        let start = task_env::process_start_time(pid as i32).unwrap();
        let cwd = env.workspace_root.join("checkout");
        let model = json!({"providerID":"openai","id":"protocol-fixture"});
        // This process hosts the local query peer. Its actual PID/start time
        // exercise the existing runtime identity checks without a native child.
        let runtime = json!({"instance":env.instance_id,"lifecycle_owner":owner,"pid":pid,"start_time":start,"cwd":cwd,"model":model});
        util::atomic_write_json(&root.join("runtime.json"), &runtime).unwrap();
        let state = json!({"adapter":"codex","receiver_kind":"task-runtime","receiver_activation":"active", "runtime":root,"state_dir":guard,"session":SESSION,
            "instance":env.instance_id,"runtime_pid":pid,"runtime_start_time":start,"cwd":cwd,"model":model});
        util::atomic_write_json(&guard.join("state.json"), &state).unwrap();
        util::atomic_write_json(
            &root.join("receiver.json"),
            &json!({"phase":"bound","guard_dir":guard,"state":state}),
        )
        .unwrap();
        let listener = UnixListener::bind(root.join("broker.sock")).unwrap();
        listener.set_nonblocking(true).unwrap();
        Self {
            env,
            queue,
            guard,
            state,
            listener,
            _registry: registry,
            directory,
        }
    }

    fn owner(&self) -> lifecycle::Owner {
        task_env::isolation::owner(&self.env).unwrap()
    }

    fn successful_reply(&self) -> J {
        json!({"result":{"thread":{"id":self.state["session"],"cwd":self.state["cwd"],"modelProvider":"openai","status":{"type":"idle"},"canAcceptDirectInput":true}}})
    }

    fn no_rpc(&self) {
        assert_eq!(
            self.listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }

    fn finish(self) {
        self.env.release_ownership();
        let state = queue::load_queue(&self.queue).unwrap();
        let owner = self.owner();
        action(
            &self.queue,
            "task-finalize",
            &json!({"task":"task","queue_id":owner.queue_id,"epoch":owner.epoch,"instance_id":owner.instance_id,
            "state_version":state["tasks"]["task"]["state_version"],"outcome":"failure","reason":"explicit no-model observation fixture teardown"}),
        );
        assert!(queue::load_queue(&self.queue).unwrap()["tasks"]["task"].is_null());
        assert!(!self.env.workspace_root.exists());
    }
}

fn action(root: &Path, name: &str, spec: &J) {
    queue::finalization::task_action(root, name, spec, Some(SESSION), &|_| {
        panic!("fixture must not contact native services")
    })
    .unwrap();
}

fn git(root: &Path, arguments: &[&str]) -> String {
    let argv: Vec<String> = std::iter::once("git")
        .chain(arguments.iter().copied())
        .map(str::to_owned)
        .collect();
    let output = util::process::run_command(
        &argv,
        &util::process::CommandOptions {
            cwd: Some(root.to_path_buf()),
            creation_mask: Some(0o077),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(output.success(), "{}", output.stderr_text());
    output.stdout_text().trim().into()
}

fn files(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut result = BTreeMap::new();
    for entry in fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            result.extend(files(&path));
        } else if path.is_file() {
            result.insert(path.clone(), fs::read(path).unwrap());
        }
    }
    result
}

fn accept(listener: &UnixListener) -> UnixStream {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match listener.accept() {
            Ok((stream, _)) => return stream,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(
                    Instant::now() < deadline,
                    "observation never queried the existing broker"
                );
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(error) => panic!("local peer accept: {error}"),
        }
    }
}

fn reply(listener: &UnixListener, value: &J) -> J {
    let mut stream = accept(listener);
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut line = String::new();
    BufReader::new(stream.try_clone().unwrap())
        .read_line(&mut line)
        .unwrap();
    let request: J = serde_json::from_str(&line).unwrap();
    assert_eq!(request["method"], "thread/read");
    assert_eq!(request["params"]["threadId"], SESSION);
    writeln!(stream, "{value}").unwrap();
    request
}

#[test]
fn exclusive_owner_gate_defers_tick_without_rpc_or_writes_then_same_path_resumes() {
    let f = Fixture::new();
    let gate = lifecycle::acquire(&f.owner(), true).unwrap();
    let before = files(f.directory.path());
    assert!(!observe_tick(&f.guard).unwrap());
    assert_eq!(
        observe(&f.guard).unwrap_err().kind,
        crate::ErrorKind::StateBusy
    );
    f.no_rpc();
    assert_eq!(files(f.directory.path()), before);
    drop(gate);
    let response = f.successful_reply();
    std::thread::scope(|scope| {
        let peer = scope.spawn(|| reply(&f.listener, &response));
        assert!(!observe_tick(&f.guard).unwrap());
        assert_eq!(peer.join().unwrap()["params"]["includeTurns"], false);
    });
    f.no_rpc();
    assert_eq!(files(f.directory.path()), before);
    f.finish();
}

#[test]
fn invalid_owner_is_fatal_for_tick_and_manual_observation_before_rpc_or_writes() {
    let f = Fixture::new();
    let original = util::read_state_json(&f.guard, "lifecycle-owner.json").unwrap();
    let mut foreign = original.clone();
    foreign["instance_id"] = json!("foreign-instance");
    util::atomic_write_json(&f.guard.join("lifecycle-owner.json"), &foreign).unwrap();
    let before = files(f.directory.path());
    let error = observe_tick(&f.guard).unwrap_err();
    assert_eq!(error.kind, crate::ErrorKind::Guard);
    assert!(error.message.contains("lifecycle owner tuple changed"));
    assert_eq!(observe(&f.guard).unwrap_err().kind, crate::ErrorKind::Guard);
    f.no_rpc();
    assert_eq!(files(f.directory.path()), before);
    util::atomic_write_json(&f.guard.join("lifecycle-owner.json"), &original).unwrap();
    f.finish();
}

#[test]
fn native_rpc_failure_remains_fatal_after_the_owner_permit_is_acquired() {
    let f = Fixture::new();
    let before = files(f.directory.path());
    let response =
        json!({"error":{"code":-32001,"message":"original receiver thread unavailable"}});
    std::thread::scope(|scope| {
        let peer = scope.spawn(|| reply(&f.listener, &response));
        let error = observe_tick(&f.guard).unwrap_err();
        assert_eq!(error.kind, crate::ErrorKind::Rpc);
        assert_eq!(error.rpc_code, Some(-32001));
        assert_eq!(error.message, "original receiver thread unavailable");
        peer.join().unwrap();
    });
    f.no_rpc();
    assert_eq!(files(f.directory.path()), before);
    f.finish();
}
