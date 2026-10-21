//! Isolated broker processes and sockets; no real model or user task resources.
pub mod common;
use herdr_flow::{task_env::MARKER_ENV, util};
use serde_json::json;
use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::{UnixListener, UnixStream},
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

struct Broker(Child);
impl Drop for Broker {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn spawn(root: &Path) -> Broker {
    let binary = std::env::var_os("HERDR_TEST_RELEASE_BIN")
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_herdr-flow").into());
    Broker(
        Command::new(binary)
            .args(["task-runtime-broker", "--state-dir"])
            .arg(root)
            .env(MARKER_ENV, "broker-fixture")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    )
}
fn ready(root: &Path, broker: &mut Broker) {
    let start = Instant::now();
    loop {
        assert!(
            broker.0.try_wait().unwrap().is_none(),
            "broker exited before readiness"
        );
        if let Ok(owner) = util::read_state_json(root, "broker-owner.json") {
            if owner["pid"] == broker.0.id() {
                return;
            }
        }
        assert!(start.elapsed() < Duration::from_secs(3));
        std::thread::sleep(Duration::from_millis(5));
    }
}
fn healthy(root: &Path) {
    let mut stream = UnixStream::connect(root.join("broker.sock")).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    writeln!(stream, "{}", json!({"method":"unsupported","params":{}})).unwrap();
    let mut response = String::new();
    BufReader::new(stream).read_line(&mut response).unwrap();
    assert!(
        response.contains("unsupported broker operation"),
        "{response}"
    );
}
fn fixture() -> tempfile::TempDir {
    let dir = common::tempdir().unwrap();
    util::atomic_write_json(
        &dir.path().join("runtime.json"),
        &json!({"instance":"broker-fixture"}),
    )
    .unwrap();
    dir
}
fn exited(broker: &mut Broker) -> std::process::ExitStatus {
    let start = Instant::now();
    loop {
        if let Some(status) = broker.0.try_wait().unwrap() {
            return status;
        }
        assert!(start.elapsed() < Duration::from_secs(3));
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn bad_connections_do_not_terminate_broker() {
    let dir = fixture();
    let root = dir.path();
    let mut broker = spawn(root);
    ready(root, &mut broker);
    for bytes in [b"{invalid}\n".as_slice(), b"\n", b"{}", b""] {
        let mut stream = UnixStream::connect(root.join("broker.sock")).unwrap();
        stream.write_all(bytes).unwrap();
        drop(stream);
        healthy(root);
    }
    let mut stream = UnixStream::connect(root.join("broker.sock")).unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let _ = stream.write_all(&vec![b' '; 1024 * 1024 + 1]);
    drop(stream);
    healthy(root);
    // A response to this request cannot be delivered; the next client still works.
    let mut stream = UnixStream::connect(root.join("broker.sock")).unwrap();
    writeln!(stream, "{}", json!({"method":"unsupported","params":{}})).unwrap();
    drop(stream);
    healthy(root);
}

#[test]
fn active_owner_is_preserved_and_killed_owner_can_be_recovered() {
    let dir = fixture();
    let root = dir.path();
    let mut first = spawn(root);
    ready(root, &mut first);
    let mut duplicate = spawn(root);
    assert_eq!(exited(&mut duplicate).code(), Some(2));
    healthy(root);
    first.0.kill().unwrap();
    first.0.wait().unwrap();
    assert!(root.join("broker.sock").exists());
    let mut recovered = spawn(root);
    ready(root, &mut recovered);
    healthy(root);
    // Identity failure stops the owner and cleans only its own socket.
    util::atomic_write_json(&root.join("runtime.json"), &json!({"instance":"other"})).unwrap();
    assert_eq!(exited(&mut recovered).code(), Some(2));
    assert!(!root.join("broker.sock").exists());
    assert_eq!(
        util::read_state_json(root, "broker-status.json").unwrap()["phase"],
        "failed"
    );
}

#[test]
fn replaced_socket_survives_old_owner_exit() {
    let dir = fixture();
    let root = dir.path();
    let mut broker = spawn(root);
    ready(root, &mut broker);
    std::fs::rename(root.join("broker.sock"), root.join("old.sock")).unwrap();
    let _replacement = UnixListener::bind(root.join("broker.sock")).unwrap();
    assert_eq!(exited(&mut broker).code(), Some(2));
    assert!(root.join("broker.sock").exists());
    let mut refused = spawn(root);
    assert_eq!(exited(&mut refused).code(), Some(2));
    assert!(root.join("broker.sock").exists());
}

#[test]
fn corrupt_journal_and_foreign_socket_fail_closed() {
    for invalid in [
        json!({}),
        json!({"turns":null,"pending":{}}),
        json!({"turns":{},"pending":[]}),
        json!({"turns":false,"pending":{}}),
    ] {
        let dir = fixture();
        let root = dir.path();
        util::atomic_write_json(&root.join("broker-events.json"), &invalid).unwrap();
        let mut broker = spawn(root);
        assert_eq!(exited(&mut broker).code(), Some(2));
        assert!(!root.join("broker.sock").exists());
        assert_eq!(
            util::read_state_json(root, "broker-events.json").unwrap(),
            invalid
        );
    }
    let dir = fixture();
    let root = dir.path();
    std::fs::write(root.join("keep"), "untouched").unwrap();
    std::os::unix::fs::symlink(root.join("keep"), root.join("broker.sock")).unwrap();
    let mut broker = spawn(root);
    assert_eq!(exited(&mut broker).code(), Some(2));
    assert!(root.join("broker.sock").is_symlink());
    assert_eq!(
        std::fs::read_to_string(root.join("keep")).unwrap(),
        "untouched"
    );
}

#[test]
fn stalled_client_releases_service_at_request_deadline() {
    use std::io::Read;
    let dir = fixture();
    let root = dir.path();
    let mut broker = spawn(root);
    ready(root, &mut broker);
    let mut stream = UnixStream::connect(root.join("broker.sock")).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(18)))
        .unwrap();
    let start = Instant::now();
    assert_eq!(stream.read(&mut [0; 1]).unwrap(), 0);
    assert!(start.elapsed() < Duration::from_secs(17));
    healthy(root);
}

#[test]
fn journal_storage_failure_stops_service_before_native_operation() {
    let dir = fixture();
    let root = dir.path();
    let mut broker = spawn(root);
    ready(root, &mut broker);
    std::fs::create_dir(root.join("broker-events.json")).unwrap();
    let mut stream = UnixStream::connect(root.join("broker.sock")).unwrap();
    writeln!(stream, "{}", json!({"method":"turn/start","params":{}})).unwrap();
    assert_eq!(exited(&mut broker).code(), Some(2));
    assert!(!root.join("broker.sock").exists());
    assert!(root.join("broker-events.json").is_dir());
}

#[test]
fn losing_the_ownership_lock_stops_broker() {
    let dir = fixture();
    let root = dir.path();
    let mut broker = spawn(root);
    ready(root, &mut broker);
    std::fs::rename(root.join("broker.lock"), root.join("old.lock")).unwrap();
    std::fs::write(root.join("broker.lock"), b"replacement").unwrap();
    assert_eq!(exited(&mut broker).code(), Some(2));
    assert!(!root.join("broker.sock").exists());
    assert_eq!(
        std::fs::read(root.join("broker.lock")).unwrap(),
        b"replacement"
    );
}
