use super::super::{
    observe_cache::Cache,
    observe_source::Source,
    observe_state::{populate, Snapshot},
    observe_wire::Wire,
};
use super::*;
use serde_json::{json, Value};
use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixListener,
    time::Duration,
};

fn registered() -> Binding {
    let mut details = binding();
    details.herdr_location.pane = "wR:p1".into();
    details.session = Some(Session {
        agent: "codex".into(),
        source: "source".into(),
        kind: "id".into(),
        value: "session".into(),
    });
    Binding {
        id: 1,
        task_id: 1,
        details,
        recorded_at: "now".into(),
    }
}
fn agent() -> Value {
    let b = registered();
    json!({"agent":"codex","workspace_id":"wR","pane_id":"wR:p1","agent_session":b.details.session,"agent_status":"working","state_change_seq":1,"completion_seq":0})
}
fn snapshot() -> Snapshot {
    let mut s = Snapshot::unknown(registered(), 10000, 2000, "test");
    s.service_instance = Some("instance".into());
    populate(&mut s, &agent());
    s
}
#[test]
fn observer_state_is_not_a_task_outcome_and_context_is_required() {
    for (input, expected) in [
        ("working", "working"),
        ("blocked", "blocked"),
        ("idle", "idle"),
        ("done", "idle"),
        ("future-state", "unknown"),
    ] {
        let mut s = snapshot();
        let mut a = agent();
        a["agent_status"] = json!(input);
        populate(&mut s, &a);
        assert_eq!(s.observed_state, expected);
        assert!(!s.completion_transition);
    }
    for field in ["pane_id", "workspace_id", "agent", "agent_session"] {
        let mut a = agent();
        a[field] = Value::Null;
        let mut s = Snapshot::unknown(registered(), 1, 10, "test");
        populate(&mut s, &a);
        assert_eq!(s.observed_state, "unknown");
    }
    let mut b = registered();
    b.details.session = None;
    let mut s = Snapshot::unknown(b, 1, 10, "test");
    populate(&mut s, &agent());
    assert_eq!(s.observed_state, "unknown");
}
#[test]
fn observer_continuity_resets_on_gap_instance_binding_expiry_or_sequence_reset() {
    let prior = snapshot();
    let mut next = prior.clone();
    next.completion_seq = Some(1);
    next.reconcile(Some(&prior), true);
    assert!(next.completion_transition);
    let mut next = prior.clone();
    next.reconcile(Some(&prior), true);
    assert!(!next.completion_transition);
    for kind in 0..5 {
        let mut next = prior.clone();
        next.completion_seq = Some(1);
        match kind {
            0 => next.service_instance = Some("replacement".into()),
            1 => next.binding.details.session.as_mut().unwrap().value = "replacement".into(),
            2 => next.observed_at_ms = 20000,
            3 => next.service_instance = None,
            _ => (),
        }
        next.reconcile(Some(&prior), kind != 4);
        assert!(!next.completion_transition);
    }
    let mut previous = prior.clone();
    previous.completion_seq = Some(10);
    let mut next = prior.clone();
    next.reconcile(Some(&previous), true);
    assert_eq!(next.continuity, "sequence_reset");
    let mut next = prior.clone();
    next.completion_seq = None;
    next.reconcile(Some(&prior), true);
    assert!(!next.completion_transition);
}
#[test]
fn observer_age_and_clock_rollback_are_unknown() {
    for now in [0, 12001] {
        let mut s = snapshot();
        s.current(now);
        assert_eq!(s.observed_state, "unknown");
    }
    let mut s = snapshot();
    s.current(12000);
    assert_eq!(s.observed_state, "working");
}
fn server(replies: Vec<Value>) -> (TempDir, PathBuf, std::thread::JoinHandle<Vec<Value>>) {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("socket");
    let listener = UnixListener::bind(&path).unwrap();
    listener.set_nonblocking(true).unwrap();
    let handle = std::thread::spawn(move || {
        let mut requests = Vec::new();
        for reply in replies {
            let deadline = std::time::Instant::now() + Duration::from_secs(4);
            let mut stream = loop {
                match listener.accept() {
                    Ok((s, _)) => break s,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(std::time::Instant::now() < deadline);
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(e) => panic!("{e}"),
                }
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut line = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap();
            requests.push(serde_json::from_str(&line).unwrap());
            writeln!(stream, "{}", reply).unwrap();
        }
        requests
    });
    (temp, path, handle)
}
#[test]
fn observer_uses_one_api_for_any_version_and_ignores_extra_fields() {
    for version in ["0.0.1", "999.3.7", "development-build", ""] {
        let (_temp, path, handle) = server(vec![
            json!({"id":"light-observe","result":{"type":"pong","version":version,"new_field":true}}),
            json!({"id":"light-observe","result":{"type":"agent_info","agent":agent()}}),
        ]);
        let source = Source {
            socket: path,
            timeout: Duration::from_millis(500),
        };
        let s = source.read(registered(), 1000, 2000);
        assert_eq!(s.observed_state, "working");
        assert_eq!(s.server_version.as_deref(), Some(version));
        let requests = handle.join().unwrap();
        assert_eq!(requests[0]["method"], "ping");
        assert_eq!(requests[1]["method"], "agent.get");
        assert_eq!(requests[1]["params"], json!({"target":"wR:p1"}));
    }
}
#[test]
fn observer_subscribes_only_registered_pane_and_rejects_error_or_wrong_id() {
    let (_temp, path, handle) = server(vec![
        json!({"id":"light-observe","result":{"type":"subscription_started"}}),
    ]);
    let source = Source {
        socket: path,
        timeout: Duration::from_millis(500),
    };
    drop(source.subscribe(&registered()).unwrap());
    let requests = handle.join().unwrap();
    assert_eq!(
        requests[0]["params"],
        json!({"subscriptions":[{"type":"pane.agent_status_changed","pane_id":"wR:p1"}]})
    );
    for reply in [
        json!({"id":"wrong","result":{}}),
        json!({"id":"light-observe","error":{"code":"unsupported_method"}}),
    ] {
        let (_temp, path, handle) = server(vec![reply]);
        let source = Source {
            socket: path,
            timeout: Duration::from_millis(500),
        };
        assert!(source.subscribe(&registered()).is_err());
        handle.join().unwrap();
    }
}
#[test]
fn observer_wire_is_bounded_and_disconnect_is_not_a_state() {
    let (_temp, path, handle) = server(vec![json!({"padding":"x".repeat(70000)})]);
    let mut wire = Wire::connect(&path, Duration::from_millis(500)).unwrap();
    assert!(wire.request("ping", json!({})).is_err());
    let _ = handle.join();
    assert!(Wire::connect(std::path::Path::new("relative"), Duration::from_millis(50)).is_err());
    let source = Source {
        socket: PathBuf::from("/missing/light-test-socket"),
        timeout: Duration::from_millis(50),
    };
    assert_eq!(source.read(registered(), 1, 2).observed_state, "unknown");
}
#[test]
fn observer_cache_dedup_budget_and_unknown_reservations_survive_reopen() {
    let f = Fixture::new();
    let options = Options::new(f.root());
    let mut cache = Cache::open(&options, false).unwrap();
    let s = snapshot();
    for _ in 0..100 {
        cache.put(&s).unwrap();
    }
    assert_eq!(f.store.events(1, Page::default()).unwrap().len(), 0);
    let id = cache.reserve(1, "episode1", 100000).unwrap().0.unwrap();
    assert!(cache.reserve(1, "episode1", 100000).unwrap().0.is_none());
    assert_eq!(cache.notifications(1).unwrap()[0]["status"], "unknown");
    cache.finish(id, "accepted", "test").unwrap();
    drop(cache);
    let mut cache = Cache::open(&options, false).unwrap();
    assert!(cache.reserve(1, "episode1", 100000).unwrap().0.is_none());
    for key in ["episode2", "episode3"] {
        assert!(cache.reserve(1, key, 100000).unwrap().0.is_some());
    }
    assert!(cache.reserve(1, "episode4", 100000).unwrap().0.is_none());
    assert_eq!(cache.notifications(1).unwrap().as_array().unwrap().len(), 3);
    let snapshot = cache.get(1).unwrap().unwrap();
    assert_eq!(snapshot.observed_state, "working");
    assert!(cache.reserve(1, "episode4", 4000000).unwrap().0.is_some());
    let read = Cache::open(&options, true).unwrap();
    assert_eq!(read.get(1).unwrap().unwrap().binding, registered());
}

#[test]
fn observer_bark_local_substitute_classifies_and_never_stores_key() {
    use super::super::observe_notify;
    use std::{io::Read, net::TcpListener, os::unix::fs::PermissionsExt};
    let temp = tempfile::tempdir().unwrap();
    let key = temp.path().join("key");
    fs::write(&key, "LOCAL_SECRET_174").unwrap();
    fs::set_permissions(&key, fs::Permissions::from_mode(0o600)).unwrap();
    for (http, body, expected) in [
        (200, r#"{"code":200}"#, "accepted"),
        (403, r#"{"code":403,"echo":"LOCAL_SECRET_174"}"#, "failed"),
        (200, "LOCAL_SECRET_174 malformed", "unknown"),
        (200, r#"{"code":400}"#, "failed"),
        (302, "", "unknown"),
    ] {
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
            let mut bytes = vec![0; length];
            reader.read_exact(&mut bytes).unwrap();
            let request: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(request["device_key"], "LOCAL_SECRET_174");
            assert!(
                !request["body"].as_str().unwrap().contains("session")
                    || request["body"]
                        .as_str()
                        .unwrap()
                        .contains("registered session")
            );
            write!(
                stream,
                "HTTP/1.1 {http} Response\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        });
        assert_eq!(observe_notify::send(&key, &endpoint, 1, 2).status, expected);
        server.join().unwrap();
    }
    assert_eq!(
        observe_notify::send(
            &temp.path().join("missing"),
            "http://127.0.0.1:1/push",
            1,
            2
        )
        .status,
        "failed"
    );
    fs::set_permissions(&key, fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(
        observe_notify::send(&key, "http://127.0.0.1:1/push", 1, 2).status,
        "failed"
    );
    for url in [
        "https://key@host/push",
        "http://remote/push",
        "https://host/key",
        "https://host/push?key=secret",
    ] {
        assert!(!observe_notify::valid_endpoint(url));
    }
}

#[test]
fn observer_notification_disconnect_and_timeout_remain_unknown() {
    use super::super::observe_notify;
    use std::{net::TcpListener, os::unix::fs::PermissionsExt};
    let temp = tempfile::tempdir().unwrap();
    let key = temp.path().join("key");
    fs::write(&key, "LOCAL_ONLY").unwrap();
    fs::set_permissions(&key, fs::Permissions::from_mode(0o600)).unwrap();
    for delay in [Duration::ZERO, Duration::from_millis(3200)] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}/push", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (_stream, _) = listener.accept().unwrap();
            std::thread::sleep(delay);
        });
        let start = std::time::Instant::now();
        assert_eq!(
            observe_notify::send(&key, &endpoint, 1, 1).status,
            "unknown"
        );
        assert!(start.elapsed() < Duration::from_secs(5));
        server.join().unwrap();
    }
}
#[test]
fn observer_cache_enforces_private_files_and_single_writer_lease() {
    use std::os::unix::fs::{symlink, PermissionsExt};
    let f = Fixture::new();
    let options = Options::new(f.root());
    let cache = Cache::open(&options, false).unwrap();
    let lease = cache.lease(1).unwrap();
    assert!(cache.lease(1).is_err());
    drop(lease);
    assert!(cache.lease(1).is_ok());
    drop(cache);
    let db = f.root().join("observations.sqlite3");
    fs::set_permissions(&db, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(Cache::open(&options, false).is_err());
    fs::set_permissions(&db, fs::Permissions::from_mode(0o600)).unwrap();
    let target = f.temp.path().join("moved.sqlite3");
    fs::rename(&db, &target).unwrap();
    symlink(&target, &db).unwrap();
    assert!(Cache::open(&options, false).is_err());
}

#[test]
fn observer_blocked_latch_survives_unknown_and_clears_only_on_observed_nonblocked() {
    let f = Fixture::new();
    let mut cache = Cache::open(&Options::new(f.root()), false).unwrap();
    let mut s = snapshot();
    s.observed_state = "blocked".into();
    cache.put(&s).unwrap();
    assert!(cache.reserve(1, "context", 1000).unwrap().0.is_some());
    for state in ["blocked", "unknown", "blocked"] {
        s.observed_state = state.into();
        s.state_change_seq = Some(100);
        cache.put(&s).unwrap();
        assert_eq!(
            cache.reserve(1, "context", 2000).unwrap(),
            (None, "deduplicated")
        );
    }
    s.observed_state = "working".into();
    cache.put(&s).unwrap();
    s.observed_state = "blocked".into();
    cache.put(&s).unwrap();
    assert!(cache.reserve(1, "context", 3000).unwrap().0.is_some());
}
