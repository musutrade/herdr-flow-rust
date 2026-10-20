//! Opt-in transport acceptance against a separately launched installed Codex.
pub mod common;

use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use herdr_flow::codex::rpc::Client;
use serde_json::json;

struct Process(Child);

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
#[ignore = "requires an installed Codex; launches a separate local app-server"]
fn installed_codex_unix_rpc() {
    let root = common::tempdir().unwrap();
    let socket = root.path().join("control.sock");
    let executable = std::env::var_os("HERDR_TEST_CODEX").unwrap_or_else(|| "codex".into());
    let version = Command::new(&executable).arg("--version").output().unwrap();
    assert!(version.status.success());
    let mut process = Process(
        Command::new(executable)
            .args(["app-server", "--listen"])
            .arg(format!("unix://{}", socket.display()))
            .current_dir(root.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while !socket.exists() {
        assert!(process.0.try_wait().unwrap().is_none(), "app-server exited");
        assert!(Instant::now() < deadline, "socket startup deadline");
        thread::sleep(Duration::from_millis(20));
    }
    let mut client = Client::with_limits(socket.to_str().unwrap(), 10.0, 16 * 1024 * 1024, 4096);
    client.connect().unwrap();
    assert!(client.initialization.as_ref().unwrap().is_object());
    let started = client
        .call(
            "thread/start",
            &json!({
                "cwd":root.path(), "ephemeral":true, "sandbox":"read-only",
                "approvalPolicy":"never"
            }),
        )
        .unwrap();
    assert_eq!(started["thread"]["ephemeral"], true);
    let id = started["thread"]["id"].as_str().unwrap();
    let observed = client
        .call("thread/read", &json!({"threadId":id,"includeTurns":false}))
        .unwrap();
    assert_eq!(observed["thread"]["id"], id);
    assert_eq!(observed["thread"]["cwd"], json!(root.path()));
    assert!(client
        .call(
            "thread/read",
            &json!({"threadId":"nonexistent-acceptance-thread"})
        )
        .is_err());
    // A new connection must initialize again and retain access to this exact
    // in-memory thread. No work turn or model request is submitted.
    let mut reconnected = Client::new(socket.to_str().unwrap());
    assert_eq!(
        reconnected
            .call("thread/read", &json!({"threadId":id}))
            .unwrap()["thread"]["id"],
        id
    );
    println!("{}: Unix upgrade, initialize, ephemeral thread/read, identity, invalid-thread rejection and reconnect passed", String::from_utf8_lossy(&version.stdout).trim());
}
