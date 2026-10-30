use crate::support::*;
use serde_json::{json, Value};
use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixListener,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};
struct Mock {
    socket: std::path::PathBuf,
    stop: Arc<AtomicBool>,
    reads: Arc<AtomicU64>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl Mock {
    fn new(f: &Fixture, state: &str, event_error: bool) -> Self {
        let socket = f.temp.path().join("herdr.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let reads = Arc::new(AtomicU64::new(0));
        let stopped = stop.clone();
        let count = reads.clone();
        let state = state.to_string();
        let worker = std::thread::spawn(move || {
            let mut subscriptions = Vec::new();
            while !stopped.load(Ordering::SeqCst) {
                let (mut stream, _) = match listener.accept() {
                    Ok(v) => v,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(e) => panic!("{e}"),
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut line = String::new();
                BufReader::new(stream.try_clone().unwrap())
                    .read_line(&mut line)
                    .unwrap();
                let request: Value = serde_json::from_str(&line).unwrap();
                let result = match request["method"].as_str().unwrap() {
                    "ping" => json!({"type":"pong","version":"future-version-not-a-gate"}),
                    "agent.get" => {
                        assert_eq!(request["params"]["target"], "wR:p1");
                        count.fetch_add(1, Ordering::SeqCst);
                        json!({"type":"agent_info","agent":{"agent":"codex","workspace_id":"wR","pane_id":"wR:p1","agent_session":{"agent":"codex","source":"mock","kind":"id","value":"registered-session"},"agent_status":state,"state_change_seq":42,"completion_seq":5}})
                    }
                    "events.subscribe" => {
                        assert_eq!(
                            request["params"],
                            json!({"subscriptions":[{"type":"pane.agent_status_changed","pane_id":"wR:p1"}]})
                        );
                        json!({"type":"subscription_started"})
                    }
                    method => panic!("forbidden method {method}"),
                };
                writeln!(stream, "{}", json!({"id":"light-observe","result":result})).unwrap();
                if request["method"] == "events.subscribe" {
                    for _ in 0..3 {
                        writeln!(stream,"{}",json!({"event":{"type":"pane.agent_status_changed","agent_status":"done"}})).unwrap();
                    }
                    if event_error {
                        writeln!(
                            stream,
                            "{}",
                            json!({"id":"light-observe","error":{"code":"events_lost"}})
                        )
                        .unwrap();
                    } else {
                        subscriptions.push(stream);
                    }
                }
            }
        });
        Self {
            socket,
            stop,
            reads,
            worker: Some(worker),
        }
    }
}
impl Drop for Mock {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.worker.take().unwrap().join().unwrap();
    }
}
fn setup(f: &Fixture, session: bool) {
    let p = f.init_project();
    assert_eq!(p, 1);
    f.ok(
        &["task", "add", "--input", "-"],
        Some(&task("add", p, Some("controlled"))),
    );
    let mut b = attach("bind", 1, 1, "developer");
    b["change"]["binding"]["herdr_location"]["pane"] = json!("wR:p1");
    if session {
        b["change"]["binding"]["session"] =
            json!({"agent":"codex","source":"mock","kind":"id","value":"registered-session"});
    }
    f.ok(&["binding", "attach", "--input", "-"], Some(&b));
}
fn args<'a>(socket: &'a str, mode: &'a str) -> Vec<&'a str> {
    vec![
        "observe",
        mode,
        "--project",
        "1",
        "--task",
        "1",
        "--binding",
        "1",
        "--socket",
        socket,
        "--service",
        "offline",
        "--interval-ms",
        "100",
        "--timeout-ms",
        "100",
        "--stale-ms",
        "300",
    ]
}
#[test]
fn observer_once_show_and_queries_leave_reported_state_and_events_unchanged() {
    let f = Fixture::new();
    setup(&f, true);
    let mock = Mock::new(&f, "working", false);
    let before = f.ok(&["task", "show", "--project", "1", "--task", "1"], None);
    let v = f.ok(&args(path(&mock.socket), "--once"), None);
    assert_eq!(v["observation"]["observed_state"], "working");
    assert_eq!(v["notification"], "not_configured");
    assert_eq!(
        before,
        f.ok(&["task", "show", "--project", "1", "--task", "1"], None)
    );
    let cached = f.ok(
        &[
            "observe",
            "--show",
            "--project",
            "1",
            "--task",
            "1",
            "--binding",
            "1",
        ],
        None,
    );
    assert_eq!(cached["observation"]["observed_state"], "working");
    assert_eq!(mock.reads.load(Ordering::SeqCst), 1);
    std::thread::sleep(Duration::from_millis(330));
    let cached = f.ok(
        &[
            "observe",
            "--show",
            "--project",
            "1",
            "--task",
            "1",
            "--binding",
            "1",
        ],
        None,
    );
    assert_eq!(cached["observation"]["observed_state"], "unknown");
    f.no_calls();
}
#[test]
fn observer_watch_treats_duplicates_as_hints_and_lost_events_as_gap() {
    for lost in [false, true] {
        let f = Fixture::new();
        setup(&f, true);
        let mock = Mock::new(&f, "blocked", lost);
        let mut a = args(path(&mock.socket), "--watch");
        a.extend(["--samples", "3"]);
        let output = f.run(&a, None);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let lines: Vec<Value> = String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .map(|s| serde_json::from_str(s).unwrap())
            .collect();
        assert_eq!(lines.len(), 4);
        for line in &lines[..3] {
            assert_eq!(line["data"]["observation"]["observed_state"], "blocked");
            assert_eq!(line["data"]["observation"]["completion_transition"], false);
        }
        if lost {
            assert_eq!(lines[1]["data"]["observation"]["continuity"], "events_lost");
        }
        assert_eq!(lines[3]["data"]["observer"], "stopped");
        assert_eq!(mock.reads.load(Ordering::SeqCst), 3);
        let query = f.ok(&["task", "show", "--project", "1", "--task", "1"], None);
        assert_eq!(query["task"]["reported_state"], "open");
        assert_eq!(query["events"]["items"].as_array().unwrap().len(), 2);
    }
}
#[test]
fn observer_missing_session_and_invalid_scope_never_connect() {
    let f = Fixture::new();
    setup(&f, false);
    let value = f.ok(&args("/missing/socket", "--once"), None);
    assert_eq!(value["observation"]["reason"], "registered_session_missing");
    let mut a = args("/missing/socket", "--once");
    a[3] = "2";
    error(f.run(&a, None), 3, "not_found");
    let a = [
        "observe",
        "--once",
        "--watch",
        "--project",
        "1",
        "--task",
        "1",
        "--binding",
        "1",
    ];
    error(f.run(&a, None), 2, "usage");
    f.no_calls();
}

#[test]
fn observer_trace_connects_only_explicit_socket_and_never_legacy_or_control_processes() {
    use std::fs;
    let f = Fixture::new();
    setup(&f, true);
    let mock = Mock::new(&f, "idle", false);
    let legacy = f.home.join(".herdr/flow");
    fs::create_dir_all(&legacy).unwrap();
    fs::write(legacy.join("queue.json"), "sentinel").unwrap();
    let trace = f.temp.path().join("observe.strace");
    let mut cmd = f.command("/usr/bin/strace");
    cmd.args([
        "-f",
        "-qq",
        "-yy",
        "-s",
        "4096",
        "-e",
        "trace=%file,%process,%network,getdents64",
        "-o",
    ])
    .arg(&trace)
    .arg("--")
    .arg(env!("CARGO_BIN_EXE_herdr-flow"))
    .args(["light", "--data-root"])
    .arg(&f.data)
    .args(args(path(&mock.socket), "--once"));
    let value = success(output(cmd, None));
    assert_eq!(value["observation"]["observed_state"], "idle");
    let calls = fs::read_to_string(trace).unwrap();
    assert_eq!(calls.matches("execve(").count(), 1);
    for forbidden in [
        "clone(",
        "clone3(",
        "fork(",
        "vfork(",
        "getdents64(",
        "queue.json",
        ".herdr/flow",
    ] {
        assert!(!calls.contains(forbidden), "{calls}");
    }
    let connects: Vec<_> = calls
        .lines()
        .filter(|line| line.contains("connect("))
        .collect();
    assert_eq!(connects.len(), 2);
    assert!(connects
        .iter()
        .all(|line| line.contains(path(&mock.socket))));
    assert_eq!(
        fs::read_to_string(legacy.join("queue.json")).unwrap(),
        "sentinel"
    );
    f.no_calls();
}

#[test]
fn observer_killed_watch_expires_without_claiming_an_exit_record() {
    use std::process::Stdio;
    let f = Fixture::new();
    setup(&f, true);
    let mock = Mock::new(&f, "working", false);
    let mut command = f.command(env!("CARGO_BIN_EXE_herdr-flow"));
    command
        .args(["light", "--data-root"])
        .arg(&f.data)
        .args(args(path(&mock.socket), "--watch"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();
    let mut reader = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    assert!(line.contains("watch_lease_unconfirmed"));
    error(f.run(&args(path(&mock.socket), "--once"), None), 5, "busy");
    child.kill().unwrap();
    child.wait().unwrap();
    std::thread::sleep(Duration::from_millis(330));
    let cached = f.ok(
        &[
            "observe",
            "--show",
            "--project",
            "1",
            "--task",
            "1",
            "--binding",
            "1",
        ],
        None,
    );
    assert_eq!(cached["observation"]["observed_state"], "unknown");
    assert_eq!(cached["observation"]["observer"], "exited_or_unverifiable");
    // The kernel releases the same-binding lease when that process exits.
    assert_eq!(
        f.ok(&args(path(&mock.socket), "--once"), None)["observation"]["observed_state"],
        "working"
    );
}

#[test]
fn observer_cli_notification_is_deduplicated_and_queryable_without_key() {
    use std::{fs, io::Read, net::TcpListener, os::unix::fs::PermissionsExt};
    let f = Fixture::new();
    setup(&f, true);
    let mock = Mock::new(&f, "blocked", false);
    let key = f.temp.path().join("key");
    fs::write(&key, "LOCAL_CLI_SECRET_174").unwrap();
    fs::set_permissions(&key, fs::Permissions::from_mode(0o600)).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}/push", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut length = 0;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if line == "\r\n" {
                break;
            }
            if let Some(v) = line.to_lowercase().strip_prefix("content-length:") {
                length = v.trim().parse::<usize>().unwrap();
            }
        }
        let mut body = vec![0; length];
        reader.read_exact(&mut body).unwrap();
        assert!(String::from_utf8(body)
            .unwrap()
            .contains("LOCAL_CLI_SECRET_174"));
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Length: 12\r\nConnection: close\r\n\r\n{{\"code\":200}}"
        )
        .unwrap();
    });
    let mut a = args(path(&mock.socket), "--once");
    a.extend(["--bark-key-file", path(&key), "--bark-endpoint", &endpoint]);
    assert_eq!(f.ok(&a, None)["notification"], "accepted");
    server.join().unwrap();
    assert_eq!(f.ok(&a, None)["notification"], "deduplicated");
    fs::remove_file(&key).unwrap();
    let cached = f.ok(
        &[
            "observe",
            "--show",
            "--project",
            "1",
            "--task",
            "1",
            "--binding",
            "1",
        ],
        None,
    );
    assert_eq!(cached["notifications"][0]["status"], "accepted");
    assert!(!cached.to_string().contains("LOCAL_CLI_SECRET_174"));
    for name in [
        "observations.sqlite3",
        "observations.sqlite3-wal",
        "observations.sqlite3-shm",
    ] {
        if let Ok(bytes) = fs::read(f.data.join(name)) {
            assert!(!bytes
                .windows(b"LOCAL_CLI_SECRET_174".len())
                .any(|b| b == b"LOCAL_CLI_SECRET_174"));
        }
    }
    let report = f.ok(&["task", "show", "--project", "1", "--task", "1"], None);
    assert_eq!(report["task"]["reported_state"], "open");
    assert_eq!(report["task"]["revision"], 2);
}

#[test]
fn observer_busy_mapping_does_not_change_record_input_io_errors() {
    use std::{
        os::{fd::OwnedFd, unix::net::UnixStream},
        process::Stdio,
    };
    let f = Fixture::new();
    f.init_project();
    let (reader, _writer) = UnixStream::pair().unwrap();
    reader.set_nonblocking(true).unwrap();
    let input: OwnedFd = reader.into();
    let mut command = f.command(env!("CARGO_BIN_EXE_herdr-flow"));
    command
        .args(["light", "--data-root"])
        .arg(&f.data)
        .args(["task", "add", "--input", "-"])
        .stdin(Stdio::from(input));
    error(command.output().unwrap(), 6, "storage_unavailable");
    f.no_calls();
}
