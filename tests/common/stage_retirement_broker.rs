//! Exercise the production broker loop in a real, registered child process.
use super::*;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Instant;

const ROOT_ENV: &str = "HERDR_STAGE_BROKER_TEST_ROOT";
const CHILD_TEST: &str = "task_runtime::retirement::tests::faults::real_broker::broker_child";

#[test]
#[ignore = "subprocess-only production broker fixture; no model"]
fn broker_child() {
    let root = PathBuf::from(std::env::var(ROOT_ENV).unwrap());
    wait_until(|| root.join("broker-test-start").exists());
    crate::task_runtime::broker::serve(&root).unwrap();
}

fn wait_until(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !condition() {
        assert!(
            Instant::now() < deadline,
            "bounded broker observation timed out"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn child_command(fixture: &Fixture) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", CHILD_TEST, "--ignored", "--nocapture"])
        .env(ROOT_ENV, fixture.root())
        .env(task_env::MARKER_ENV, &fixture.owner.instance_id);
    command
}

fn install_real_broker(fixture: &mut Fixture) {
    fixture.broker.kill().unwrap();
    fixture.broker.wait().unwrap();
    let mut env = fixture.active();
    fixture.broker = child_command(fixture).spawn().unwrap();
    let pid = fixture.broker.id() as i32;
    let ticks = task_env::process_start_time(pid).unwrap();
    env.child_processes[0] = task_env::RegisteredProcess {
        pid,
        pgid: pid,
        start_time: Some(ticks),
        descendants: vec![],
        direct_children: vec![],
    };
    let root = fixture.root();
    let mut runtime = read(&root, "runtime.json").unwrap();
    runtime["pid"] = json!(pid);
    runtime["start_time"] = json!(ticks);
    util::atomic_write_json(&root.join("runtime.json"), &runtime).unwrap();
    let mut journal = read(&root, "upstream-session.json").unwrap();
    journal["runtime"] = runtime.clone();
    util::atomic_write_json(&root.join("upstream-session.json"), &journal).unwrap();
    let mut plan = load(&env).unwrap();
    plan.entries[0].broker.pid = pid;
    plan.entries[0].broker.start_time = Some(ticks);
    plan.entries[0].runtime = runtime;
    plan.entries[0].sessions[0] = journal;
    save(&mut env, &plan).unwrap();
    drop(env);
    fs::write(root.join("broker-test-start"), "start").unwrap();
    wait_until(|| {
        assert!(
            fixture.broker.try_wait().unwrap().is_none(),
            "broker startup exited"
        );
        read(&root, "broker-status.json").is_ok_and(|s| s["phase"] == "running")
    });
}

enum NativeCommand {
    Event(J),
    Response(J, Sender<()>),
}

struct Native {
    _directory: tempfile::TempDir,
    path: PathBuf,
    cwd: Arc<Mutex<PathBuf>>,
    active: Arc<AtomicBool>,
    commands: Option<Sender<NativeCommand>>,
    requests: Arc<Mutex<Vec<J>>>,
    stopped: Arc<AtomicBool>,
    task: Option<std::thread::JoinHandle<()>>,
}

impl Native {
    fn new() -> Self {
        let directory = util::fs::test_tempdir();
        let path = directory.path().join("native.sock");
        let listener = UnixListener::bind(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let cwd = Arc::new(Mutex::new(PathBuf::new()));
        let active = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(Mutex::new(vec![]));
        let stopped = Arc::new(AtomicBool::new(false));
        let (commands, receiver) = mpsc::channel();
        let (location, status, calls, stop) = (
            cwd.clone(),
            active.clone(),
            requests.clone(),
            stopped.clone(),
        );
        let task = std::thread::spawn(move || {
            let mut receiver = Some(receiver);
            let mut peers = vec![];
            while !stop.load(Ordering::SeqCst) {
                let (stream, _) = match listener.accept() {
                    Ok(pair) => pair,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(error) => panic!("native accept: {error}"),
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let (location, status, calls, commands) = (
                    location.clone(),
                    status.clone(),
                    calls.clone(),
                    receiver.take(),
                );
                peers.push(std::thread::spawn(move || {
                    native_peer(
                        rpc_server::Peer { stream },
                        location,
                        status,
                        calls,
                        commands,
                    );
                }));
            }
            for peer in peers {
                peer.join().unwrap();
            }
        });
        Self {
            _directory: directory,
            path,
            cwd,
            active,
            commands: Some(commands),
            requests,
            stopped,
            task: Some(task),
        }
    }

    fn event(&self, value: J) {
        self.commands
            .as_ref()
            .unwrap()
            .send(NativeCommand::Event(value))
            .unwrap();
    }
}

impl Drop for Native {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::SeqCst);
        self.commands.take();
        let joined = self.task.take().unwrap().join();
        if !std::thread::panicking() {
            joined.unwrap();
        }
    }
}

fn native_peer(
    mut peer: rpc_server::Peer,
    cwd: Arc<Mutex<PathBuf>>,
    active: Arc<AtomicBool>,
    calls: Arc<Mutex<Vec<J>>>,
    commands: Option<Receiver<NativeCommand>>,
) {
    peer.initialize();
    let request = peer.request();
    assert_eq!(request["method"], "thread/read");
    calls.lock().unwrap().push(request.clone());
    peer.json(&json!({"id":request["id"],"result":{"thread":{"id":WORKER,
        "cwd":*cwd.lock().unwrap(),"status":{"type":if active.load(Ordering::SeqCst) {"active"} else {"idle"},"activeFlags":[]}}}}));
    if let Some(commands) = commands {
        for command in commands {
            match command {
                NativeCommand::Event(value) => peer.json(&value),
                NativeCommand::Response(expected, done) => {
                    let response = peer.request();
                    assert_eq!(response, expected);
                    calls.lock().unwrap().push(response);
                    done.send(()).unwrap();
                }
            }
        }
    }
}

fn wait_turn(fixture: &mut Fixture, turn: &str, status: &str) {
    wait_until(|| {
        assert!(
            fixture.broker.try_wait().unwrap().is_none(),
            "real broker exited while its native frontend/work still existed"
        );
        read(&fixture.root(), "broker-events.json")
            .is_ok_and(|j| j["turns"][turn]["turn"]["status"] == status)
    });
}

fn drive_tick(fixture: &Fixture) -> FlowError {
    let mut result = None;
    wait_until(|| {
        let error = drive_owner(&fixture.owner).unwrap_err();
        if error.kind == crate::ErrorKind::StateBusy
            && error.message != "stage resources await the next bounded cleanup tick"
        {
            return false; // The real broker may briefly hold its original SH.
        }
        result = Some(error);
        true
    });
    result.unwrap()
}

fn prepared_checkpoint(fixture: &Fixture) -> util::FileLock {
    let root = fixture.root();
    let lock = util::FileLock::acquire(&root.join("control-service.lock"), false)
        .unwrap()
        .unwrap();
    let old = json!({"phase":"complete","result":{"success":true,"contentItems":[]}});
    let proof = util::pyjson::sha256_hex(old.to_string().as_bytes());
    util::atomic_write_json(&root.join("control-tools.json"), &json!({"old":old})).unwrap();
    util::atomic_write_json(
        &root.join("record-checkpoint.json"),
        &json!({
        "phase":"pruning","generation":1,"active":{"thread":WORKER,"turn":"previous"},
        "files":{"control-tools.json":{"old":proof}}}),
    )
    .unwrap();
    lock
}

#[test]
fn real_broker_observes_and_drains_work_after_retirement_fence_until_frontend_exit_and_idle() {
    let native = Native::new();
    let mut fixture = Fixture::new(&native.path);
    *native.cwd.lock().unwrap() = fixture.clone_path();
    let herdr = PresentationServer::new(&fixture.temp.path().join("herdr.sock"), None);
    let mut frontend = Frontend::new();
    install_presentation(&fixture, &herdr, &frontend);
    install_real_broker(&mut fixture);
    crate::task_runtime::broker::call(&fixture.root(), "thread/read", &json!({"threadId":WORKER}))
        .unwrap();
    let checkpoint_lock = prepared_checkpoint(&fixture);
    assert_eq!(drive_tick(&fixture).kind, crate::ErrorKind::StateBusy);
    assert!(fixture.root().join(MARKER).exists());
    assert!(frontend.0.try_wait().unwrap().is_none());
    drop(checkpoint_lock);
    wait_until(|| {
        read(&fixture.root(), "record-checkpoint.json").is_ok_and(|j| j["phase"] == "complete")
    });
    assert!(read(&fixture.root(), "control-tools.json").unwrap()["old"].is_null());

    // This real upstream notification arrives after the durable fence but
    // before the next watcher tick can close the still-running native TUI.
    native.active.store(true, Ordering::SeqCst);
    native.event(json!({"method":"turn/started","params":{"threadId":WORKER,
        "turn":{"id":"native-late","status":"inProgress"}}}));
    wait_turn(&mut fixture, "native-late", "inProgress");
    for method in [
        "thread/start",
        "thread/resume",
        "turn/start",
        "thread/read",
        "thread/turns/list",
    ] {
        let error =
            crate::task_runtime::broker::call(&fixture.root(), method, &json!({"threadId":WORKER}))
                .unwrap_err();
        assert!(error.message.contains("retired"), "{error}");
        assert!(fixture.broker.try_wait().unwrap().is_none());
    }
    let restarted = child_command(&fixture).output().unwrap();
    assert!(!restarted.status.success());
    assert!(String::from_utf8_lossy(&restarted.stderr).contains("retired"));
    assert!(fixture.broker.try_wait().unwrap().is_none());

    let error = drive_tick(&fixture);
    assert!(
        error.message.contains("frontend is still running"),
        "{error}"
    );
    frontend.stop();
    let error = drive_tick(&fixture);
    assert!(error.message.contains("current native work"), "{error}");
    assert!(fixture.broker.try_wait().unwrap().is_none());
    assert!(fixture.clone_path().exists());

    drain_recorded_response(&native, &mut fixture);
    native.event(
        json!({"method":"turn/completed","params":{"threadId":WORKER,
        "turn":{"id":"native-late","status":"completed"}}}),
    );
    wait_turn(&mut fixture, "native-late", "completed");
    native.active.store(false, Ordering::SeqCst);
    assert_eq!(drive_tick(&fixture).kind, crate::ErrorKind::StateBusy);
    assert_eq!(fixture.plan().entries[0].phase, "sessions");
    assert!(fixture.broker.try_wait().unwrap().is_some());
    assert!(fixture.unrelated.try_wait().unwrap().is_none());
    assert_eq!(native.requests.lock().unwrap().len(), 5);
}

fn drain_recorded_response(native: &Native, fixture: &mut Fixture) {
    let request = json!({"id":91,"method":"item/tool/call","params":{
        "threadId":WORKER,"turnId":"native-late","tool":"herdr_queue"}});
    native.event(request.clone());
    wait_until(|| {
        read(&fixture.root(), "broker-tools.json").is_ok_and(|j| j["91"]["phase"] == "pending")
    });
    let result = json!({"success":true,"contentItems":[]});
    util::atomic_write_json(
        &fixture.root().join("control-tools.json"),
        &json!({"91":{"phase":"complete","request":request,"result":result}}),
    )
    .unwrap();
    let (done, received) = mpsc::channel();
    native
        .commands
        .as_ref()
        .unwrap()
        .send(NativeCommand::Response(
            json!({"id":91,"result":result}),
            done,
        ))
        .unwrap();
    let value = crate::task_runtime::broker::call(
        &fixture.root(),
        "tool/respond",
        &json!({"id":91,"result":result}),
    )
    .unwrap();
    assert_eq!(value["responded"], true);
    received.recv_timeout(Duration::from_secs(5)).unwrap();
}
