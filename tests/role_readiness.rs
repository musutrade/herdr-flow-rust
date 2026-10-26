//! Dedicated no-model tool processes and loopback fixtures; no installed servers.
pub mod common;
use herdr_flow::{queue, util};
use serde_json::{json, Value as J};
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    process::Command,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::Duration,
};

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    cwd: PathBuf,
    spec: J,
    mode: Arc<Mutex<String>>,
    stop: Arc<AtomicBool>,
    server: Option<thread::JoinHandle<()>>,
}

fn respond(mut stream: TcpStream, mode: &str, cwd: &std::path::Path) {
    stream
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    let mut request = Vec::new();
    while !request.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        if stream.read_exact(&mut byte).is_err() {
            return;
        }
        request.push(byte[0]);
        assert!(request.len() < 16384);
    }
    let request = String::from_utf8(request).unwrap();
    assert!(request.starts_with("GET "), "readiness must never submit");
    let health = request.starts_with("GET /global/health ");
    if mode == "timeout" {
        thread::sleep(Duration::from_secs(11));
        return;
    }
    let (status, body) = match mode {
        "401" => (401, json!({"secret":"never-report-this"})),
        "403" => (403, json!({"secret":"never-report-this"})),
        "503" => (503, json!({"secret":"never-report-this"})),
        "unknown" => (200, json!({"unrecognized":"never-report-this"})),
        _ if health => (200, json!({"healthy":true,"version":"1.19.4"})),
        "unknown_session" => (200, json!({})),
        _ => (
            200,
            json!({"id":if mode == "identity" {"sesother"} else {"sesfixture"},
            "directory":cwd,"model":{"providerID":"fixture","id":"model"}}),
        ),
    };
    let body = body.to_string();
    let _ = write!(stream, "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
}

impl Fixture {
    fn new() -> Self {
        let temp = common::tempdir().unwrap();
        let root = temp.path().join("queue");
        let cwd = temp.path().join("checkout");
        let guard = temp.path().join("guard");
        common::create_dir_all(&cwd).unwrap();
        common::create_dir_all(&guard).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let state = json!({"version":2,"adapter":"opencode","session":"sesfixture",
            "cwd":cwd,"state_dir":guard,"endpoint":endpoint,"server_version":"1.19.4",
            "model":{"providerID":"fixture","id":"model"},"auto_rotate":false});
        util::atomic_write_json(&guard.join("state.json"), &state).unwrap();
        queue::init(&root, Some(&guard), 30).unwrap();
        common::create_dir_all(root.join("results")).unwrap();
        let executable = std::fs::canonicalize(env!("CARGO_BIN_EXE_herdr-flow")).unwrap();
        let spec = json!({"role":"receiver","cwd":cwd,"executable":executable,
            "sha256":util::pyjson::sha256_hex(&std::fs::read(&executable).unwrap()),
            "identity":{"adapter":"opencode","session":"sesfixture"},
            "endpoint":endpoint,"model":state["model"]});
        let mode = Arc::new(Mutex::new("normal".to_string()));
        let stop = Arc::new(AtomicBool::new(false));
        let (remote_mode, remote_stop, remote_cwd) = (mode.clone(), stop.clone(), cwd.clone());
        let server = thread::spawn(move || {
            while !remote_stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        respond(stream, &remote_mode.lock().unwrap().clone(), &remote_cwd)
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2))
                    }
                    Err(e) => panic!("fixture accept: {e}"),
                }
            }
        });
        Self {
            _temp: temp,
            root,
            cwd,
            spec,
            mode,
            stop,
            server: Some(server),
        }
    }

    fn command(&self, spec: &J) -> Command {
        let path = self.root.join("probe-spec.json");
        util::atomic_write_json(&path, spec).unwrap();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_herdr-flow"));
        cmd.args(["queue", "readiness", "--state-dir"])
            .arg(&self.root)
            .arg("--spec")
            .arg(path)
            .current_dir(&self.cwd)
            .env("OPENCODE_SESSION_ID", "sesfixture")
            .env(
                "OPENCODE_SERVER_PASSWORD",
                "fixture-credential-never-report",
            );
        cmd
    }

    fn run(&self, spec: &J) -> J {
        read_report(&mut self.command(spec))
    }

    fn mode(&self, value: &str) {
        *self.mode.lock().unwrap() = value.to_string();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.server.take().unwrap().join().unwrap();
    }
}

fn read_report(cmd: &mut Command) -> J {
    let output = cmd.output().unwrap();
    assert_eq!(
        output.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: J = serde_json::from_slice(&output.stdout).unwrap();
    let text = report.to_string();
    assert!(!text.contains("fixture-credential-never-report"));
    assert!(!text.contains("never-report-this"));
    assert_eq!(report["ready"], false);
    assert_eq!(report["active_wakeup"], false);
    report
}

fn channel(report: &J) -> &J {
    &report["observed"]["channel"]["status"]
}

#[test]
fn normal_current_tool_connection_does_not_attest_another_role_sandbox() {
    let f = Fixture::new();
    let before = std::fs::read(f.root.join("queue.json")).unwrap();
    let result = f.run(&f.spec);
    assert_eq!(channel(&result), "ready");
    assert_eq!(result["status"], "unknown");
    assert_eq!(result["context"]["repository_trust"], "unknown");
    assert_eq!(result["context"]["role_sandbox_equivalence"], "unknown");
    assert_eq!(
        result["observed"]["channel"]["authentication_source"],
        "OPENCODE_SERVER_PASSWORD"
    );
    assert_eq!(std::fs::read(f.root.join("queue.json")).unwrap(), before);
    assert!(!f.root.join("dispatch.json").exists());
    assert!(!f.root.join("submission.json").exists());
}

#[test]
fn typed_auth_policy_identity_protocol_and_timeout_failures_stay_distinct() {
    let f = Fixture::new();
    for (mode, expected) in [
        ("401", "authentication_failed"),
        ("403", "policy_denied"),
        ("503", "transport_failure"),
        ("identity", "identity_mismatch"),
        ("unknown", "unknown"),
        ("unknown_session", "unknown"),
        ("timeout", "timeout"),
    ] {
        f.mode(mode);
        assert_eq!(channel(&f.run(&f.spec)), expected, "{mode}");
    }
}

#[test]
fn configuration_identity_endpoint_executable_and_cwd_are_pinned() {
    let f = Fixture::new();
    for (key, value, expected) in [
        ("cwd", json!(f.root), "cwd_mismatch"),
        ("sha256", json!("0".repeat(64)), "executable_conflict"),
        ("executable", json!("/bin/true"), "executable_conflict"),
        ("endpoint", json!("http://127.0.0.1:1"), "endpoint_mismatch"),
        (
            "model",
            json!({"providerID":"wrong","id":"model"}),
            "identity_mismatch",
        ),
        ("model", J::Null, "missing_configuration"),
        ("role", json!("other"), "unsupported_channel"),
    ] {
        let mut spec = f.spec.clone();
        spec[key] = value;
        assert_eq!(channel(&f.run(&spec)), expected, "{key}");
    }
    let mut spec = f.spec.clone();
    spec["identity"]["session"] = json!("seswrong");
    assert_eq!(channel(&f.run(&spec)), "identity_mismatch");
    let mut spec = f.spec.clone();
    spec["identity"]["adapter"] = json!("codex");
    let mut cmd = f.command(&spec);
    cmd.env("CODEX_THREAD_ID", "sesfixture");
    assert_eq!(channel(&read_report(&mut cmd)), "unsupported_channel");
    let mut cmd = f.command(&f.spec);
    cmd.env_remove("OPENCODE_SESSION_ID");
    assert_eq!(channel(&read_report(&mut cmd)), "identity_mismatch");
    assert_eq!(channel(&f.run(&json!({}))), "missing_configuration");
}

#[test]
fn path_shadow_and_unrelated_defaults_cannot_override_absolute_arguments() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    let bin = f.cwd.join("bin");
    std::fs::create_dir(&bin).unwrap();
    let shadow = bin.join("herdr-flow");
    std::fs::write(&shadow, "#!/bin/sh\nexit 99\n").unwrap();
    std::fs::set_permissions(&shadow, std::fs::Permissions::from_mode(0o700)).unwrap();
    for default in ["endpoint=wrong-one", "endpoint=wrong-two"] {
        std::fs::write(f.cwd.join("defaults.conf"), default).unwrap();
        let mut cmd = f.command(&f.spec);
        cmd.env("PATH", &bin);
        let result = read_report(&mut cmd);
        assert_eq!(channel(&result), "ready");
        assert_eq!(
            result["observed"]["executable"]["path_lookup"],
            "conflict_overridden_by_absolute_selection"
        );
    }
}

#[test]
fn producer_needs_only_its_result_path_and_never_queries_receiver() {
    let f = Fixture::new();
    f.mode("401");
    let mut queue = queue::load_queue(&f.root).unwrap();
    queue["waits"]["w"] = json!({"kind":"signal","closed":false,
        "producer":{"adapter":"opencode","session":"sesfixture"}});
    queue::save(&f.root, &mut queue).unwrap();
    let mut spec = f.spec.clone();
    spec["role"] = json!("producer");
    spec["wait"] = json!("w");
    let result = f.run(&spec);
    assert_eq!(channel(&result), "ready");
    assert_eq!(
        result["observed"]["channel"]["control_socket"],
        "not_required"
    );
    assert_eq!(
        std::fs::read_dir(f.root.join("results")).unwrap().count(),
        0
    );
    spec["wait"] = json!("missing");
    assert_eq!(channel(&f.run(&spec)), "unsupported_channel");
}

#[cfg(target_os = "linux")]
fn deny_connect(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    // This fixture only narrows its child's policy. It never changes the parent,
    // global trust, credentials, namespaces or any installed security profile.
    let filter = [
        libc::sock_filter {
            code: (libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16,
            jt: 0,
            jf: 0,
            k: 0,
        },
        libc::sock_filter {
            code: (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16,
            jt: 0,
            jf: 1,
            k: libc::SYS_connect as u32,
        },
        libc::sock_filter {
            code: (libc::BPF_RET | libc::BPF_K) as u16,
            jt: 0,
            jf: 0,
            k: libc::SECCOMP_RET_ERRNO | libc::EACCES as u32,
        },
        libc::sock_filter {
            code: (libc::BPF_RET | libc::BPF_K) as u16,
            jt: 0,
            jf: 0,
            k: libc::SECCOMP_RET_ALLOW,
        },
    ];
    // SAFETY: the fixed filter is prepared before fork; the callback borrows
    // that memory only while prctl copies it. No allocation or locking occurs.
    unsafe {
        cmd.pre_exec(move || {
            let program = libc::sock_fprog {
                len: filter.len() as u16,
                filter: filter.as_ptr().cast_mut(),
            };
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::prctl(libc::PR_SET_SECCOMP, libc::SECCOMP_MODE_FILTER, &program) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(target_os = "linux")]
#[test]
fn parent_can_connect_but_actual_tool_process_is_denied() {
    let f = Fixture::new();
    assert_eq!(channel(&f.run(&f.spec)), "ready");
    let mut cmd = f.command(&f.spec);
    deny_connect(&mut cmd);
    let result = read_report(&mut cmd);
    assert_eq!(channel(&result), "policy_denied");
    assert_eq!(result["status"], "blocked");
    assert_eq!(channel(&f.run(&f.spec)), "ready");
}
