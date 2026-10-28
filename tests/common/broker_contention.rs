//! Real registered services defer only the Owner gate, without replaying RPC.
use super::*;
use std::{
    io::{BufRead, BufReader, Write},
    os::{fd::AsRawFd, unix::net::UnixStream},
    sync::{mpsc, Arc},
    time::{Duration, Instant},
};

struct Watcher {
    pid: i32,
    start: Option<u64>,
}
impl Drop for Watcher {
    fn drop(&mut self) {
        if task_env::process_start_time(self.pid) == self.start {
            // SAFETY: this exact task-owned child identity was returned by
            // activate; kill and reap only the synthetic test watcher.
            unsafe { libc::kill(self.pid, libc::SIGKILL) };
        }
        // SAFETY: waitpid borrows no memory; this watcher was our direct child.
        unsafe { libc::waitpid(self.pid, std::ptr::null_mut(), 0) };
    }
}

fn readable(peer: &rpc_server::Peer) -> bool {
    let mut fd = libc::pollfd {
        fd: peer.stream.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: poll borrows one initialized descriptor for a bounded check.
    let count = unsafe { libc::poll(&mut fd, 1, 20) };
    assert!(count >= 0);
    count > 0 && fd.revents & libc::POLLIN != 0
}

fn fake_upstream(peer: &mut rpc_server::Peer, cwd: &Path, requests: &Mutex<Vec<J>>) {
    let request = peer.request();
    requests.lock().unwrap().push(request.clone());
    assert!(matches!(
        request["method"].as_str(),
        Some("thread/start" | "thread/read")
    ));
    let result = json!({"thread":{"id":SESSION,"cwd":cwd,"modelProvider":"openai",
        "status":{"type":"idle"},"canAcceptDirectInput":true,"turns":[]},
        "model":"fixture","modelProvider":"openai","probe":request["params"]["probe"]});
    peer.json(&json!({"id":request["id"],"result":result}));
}

fn acquire_gate(owner: &lifecycle::Owner) -> lifecycle::Permit {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match lifecycle::acquire(owner, true) {
            Ok(permit) => return permit,
            Err(error) if error.kind == herdr_flow::ErrorKind::StateBusy => {
                assert!(
                    Instant::now() < deadline,
                    "broker retained the Owner permit"
                );
                std::thread::yield_now();
            }
            Err(error) => panic!("{error}"),
        }
    }
}

fn context_switches(pid: i32) -> u64 {
    fs::read_to_string(format!("/proc/{pid}/status"))
        .unwrap()
        .lines()
        .find_map(|line| line.strip_prefix("voluntary_ctxt_switches:"))
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

fn deferred_tick(guard: &Path, watcher: &Watcher, before: u64, broker: &mut FixtureChild) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        assert!(
            broker.0.try_wait().unwrap().is_none(),
            "broker exited during gate contention"
        );
        let health = herdr_flow::watch_service::health(guard);
        assert_eq!(health["healthy"], true, "receiver watcher exited: {health}");
        if context_switches(watcher.pid) > before {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "watcher did not yield after its deferred tick"
        );
        std::thread::yield_now();
    }
}

fn resumed_heartbeat(guard: &Path, watcher: &Watcher, before: f64) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let health = herdr_flow::watch_service::health(guard);
        assert_eq!(
            health["healthy"], true,
            "watcher failed after gate release: {health}"
        );
        assert_eq!(health["pid"], watcher.pid);
        assert_eq!(task_env::process_start_time(watcher.pid), watcher.start);
        if health["heartbeat_at"].as_f64().unwrap() > before {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "original watcher did not resume its heartbeat"
        );
        std::thread::yield_now();
    }
}

#[test]
fn registered_broker_and_receiver_survive_exclusive_gate_without_rpc_replay() {
    let f = Fixture::new();
    let mut env = f.ready();
    let root = task_runtime::prepare(&env, "contention").unwrap();
    let cwd = env.workspace_root.join("checkout");
    let server_cwd = cwd.clone();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let server_requests = requests.clone();
    let (release, released) = mpsc::channel();
    let server = rpc_server::Server::start(move |mut peer| {
        peer.initialize();
        while released.try_recv().is_err() {
            if readable(&peer) {
                fake_upstream(&mut peer, &server_cwd, &server_requests);
            }
        }
    });
    let mut broker = FixtureChild(task_runtime::launch(&mut env, &root,
        &json!({"adapter":"codex","configuration":"upstream","socket":server.path,"cwd":cwd,
            "model":{"providerID":"openai","id":"fixture"},"driver_binary":env!("CARGO_BIN_EXE_herdr-flow")})).unwrap());
    task_runtime::native::receiver::bootstrap(
        &env,
        &root,
        &json!({"guard_dir":root.join("guard"),"task":"task","role":"coordinator"}),
    )
    .unwrap();
    let active = task_runtime::native::receiver::active::activate(
        &mut env,
        &root,
        &json!({"driver_binary":env!("CARGO_BIN_EXE_herdr-flow")}),
    )
    .unwrap();
    let pid = active["pid"].as_u64().unwrap() as i32;
    let watcher = Watcher {
        pid,
        start: task_env::process_start_time(pid),
    };
    assert!(env.owns_process(pid));
    assert!(env.owns_process(broker.0.id() as i32));
    let identity = util::read_state_json(&root, "broker-owner.json").unwrap();
    let guard = root.join("guard");
    let gate = acquire_gate(&task_env::isolation::owner(&env).unwrap());
    let count = requests.lock().unwrap().len();
    let switches = context_switches(watcher.pid);
    let mut pending = UnixStream::connect(root.join("broker.sock")).unwrap();
    pending
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    writeln!(
        pending,
        "{}",
        json!({"method":"thread/read","params":{"threadId":SESSION,"probe":"same-frame"}})
    )
    .unwrap();
    // The original watcher's next yield is the barrier, not elapsed sleep.
    // It cannot send an RPC or write a heartbeat while the exclusive gate is held.
    deferred_tick(&guard, &watcher, switches, &mut broker);
    assert_eq!(requests.lock().unwrap().len(), count);
    assert_eq!(
        task_runtime::native::receiver::active::observe(&guard)
            .unwrap_err()
            .kind,
        herdr_flow::ErrorKind::StateBusy
    );
    assert_eq!(
        util::read_state_json(&root, "broker-owner.json").unwrap(),
        identity
    );
    let heartbeat = herdr_flow::watch_service::health(&guard)["heartbeat_at"]
        .as_f64()
        .unwrap();
    drop(gate);
    let mut line = String::new();
    BufReader::new(pending).read_line(&mut line).unwrap();
    let response: J = serde_json::from_str(&line).unwrap();
    assert_eq!(response["result"]["probe"], "same-frame");
    assert!(broker.0.try_wait().unwrap().is_none());
    assert_eq!(
        util::read_state_json(&root, "broker-owner.json").unwrap(),
        identity
    );
    resumed_heartbeat(&guard, &watcher, heartbeat);
    let observed = requests.lock().unwrap().clone();
    assert_eq!(observed[0]["method"], "thread/start");
    assert!(observed[1..].iter().all(|r| r["method"] == "thread/read"));
    assert_eq!(
        observed
            .iter()
            .filter(|r| r["params"]["probe"] == "same-frame")
            .count(),
        1
    );
    drop(watcher);
    release.send(()).unwrap();
    server.finish();
    drop(broker);
}
