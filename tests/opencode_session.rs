//! Native OpenCode protocol, durable submissions and queue integration fixtures.
pub mod common;

use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use herdr_flow::herdr::{receiver_factory, ReceiverAdapter};
use herdr_flow::model::J;
use herdr_flow::opencode::session::{self, SessionAdapter};
use herdr_flow::{guard, progress, queue, runtime, util};
use serde_json::{json, Value};

struct Remote {
    session: J,
    version: String,
    status: Option<J>,
    permissions: J,
    questions: J,
    messages: Vec<J>,
    posts: Vec<J>,
    lose_response: bool,
    reject_post: bool,
}

impl Remote {
    fn response(&mut self, method: &str, path: &str, body: J) -> (u16, J) {
        if method == "POST" {
            assert_eq!(path, "/session/sesfixture/prompt_async");
            self.posts.push(body.clone());
            if self.reject_post {
                return (503, json!({"error":"unavailable"}));
            }
            let mut model = body["model"].clone();
            if !body["variant"].is_null() {
                model["variant"] = body["variant"].clone();
            }
            self.messages.push(json!({"info":{"id":body["messageID"],"role":"user","sessionID":"sesfixture","model":model},"parts":body["parts"]}));
            self.status = Some(json!({"type":"busy"}));
            if self.lose_response {
                return (503, json!({"error":"response lost after accepting"}));
            }
            return (204, Value::Null);
        }
        assert_eq!(method, "GET");
        let value = match path {
            "/global/health" => json!({"healthy":true,"version":self.version}),
            "/session/sesfixture" => self.session.clone(),
            "/session/status" => self
                .status
                .as_ref()
                .map_or(json!({}), |s| json!({"sesfixture":s})),
            "/permission" => self.permissions.clone(),
            "/question" => self.questions.clone(),
            "/session/sesfixture/message" => json!(self.messages),
            _ => return (404, json!({"error":"not found"})),
        };
        (200, value)
    }

    fn finish(&mut self, error: Option<J>, finish: &str) {
        let user = self.posts.last().unwrap();
        self.messages.push(json!({"info":{"id":"msgreply","sessionID":"sesfixture","role":"assistant","parentID":user["messageID"],
            "providerID":"provider","modelID":"model","variant":user["variant"],"path":{"cwd":self.session["directory"]},"time":{"completed":1},"finish":finish,"error":error},
            "parts":[{"type":"text","text":"Review completed; inspect findings independently."}]}));
        self.status = None;
    }
}

fn serve(mut stream: TcpStream, remote: &Mutex<Remote>) {
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut headers = Vec::new();
    while !headers.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        if stream.read_exact(&mut byte).is_err() {
            return;
        }
        headers.push(byte[0]);
        assert!(headers.len() < 16384);
    }
    let headers = String::from_utf8(headers).unwrap();
    let mut request = headers.lines().next().unwrap().split_whitespace();
    let method = request.next().unwrap();
    let path = request.next().unwrap().split('?').next().unwrap();
    let length = headers
        .lines()
        .find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().unwrap())
        })
        .unwrap_or(0);
    assert!(length < 2 * 1024 * 1024);
    let mut body = vec![0; length];
    stream.read_exact(&mut body).unwrap();
    let body = if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&body).unwrap()
    };
    let (status, value) = remote.lock().unwrap().response(method, path, body);
    let body = if status == 204 {
        String::new()
    } else {
        serde_json::to_string(&value).unwrap()
    };
    let response=format!("HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len());
    let _ = stream.write_all(response.as_bytes());
}

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    cwd: PathBuf,
    state: J,
    remote: Arc<Mutex<Remote>>,
    stop: Arc<AtomicBool>,
    server: Option<JoinHandle<()>>,
}

impl Fixture {
    fn new() -> Self {
        let temp = common::tempdir().unwrap();
        let cwd = temp.path().join("workspace");
        common::create_dir(&cwd).unwrap();
        let root = temp.path().join("state");
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let remote = Arc::new(Mutex::new(Remote {
            session: json!({"id":"sesfixture","directory":cwd,"model":{"providerID":"provider","id":"model"}}),
            version: "1.19.3".into(),
            status: None,
            permissions: json!([]),
            questions: json!([]),
            messages: vec![],
            posts: vec![],
            lose_response: false,
            reject_post: false,
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let task_remote = remote.clone();
        let task_stop = stop.clone();
        let server = thread::spawn(move || {
            while !task_stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => serve(stream, &task_remote),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(1))
                    }
                    Err(error) => panic!("accept failed: {error}"),
                }
            }
        });
        let state=session::register(&root,&json!({"endpoint":endpoint,"session":"sesfixture","cwd":cwd,"model":{"providerID":"provider","id":"model"},"task":"o/r#1","role":"coordinator"})).unwrap();
        Self {
            _temp: temp,
            root,
            cwd,
            state,
            remote,
            stop,
            server: Some(server),
        }
    }

    fn adapter(&self) -> SessionAdapter {
        SessionAdapter::new(&self.state).unwrap()
    }
    fn finish(&self, error: Option<J>, finish: &str) {
        self.remote.lock().unwrap().finish(error, finish);
    }
    fn posts(&self) -> usize {
        self.remote.lock().unwrap().posts.len()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.server.take().unwrap().join().unwrap();
    }
}

#[test]
fn native_binding_status_and_exact_submission_completion() {
    let f = Fixture::new();
    let adapter = f.adapter();
    assert_eq!(adapter.agent().unwrap()["interactive_ready"], true);
    let id = adapter.prompt("review the candidate").unwrap();
    assert!(id.starts_with("msg"));
    // OpenCode 1.19.3 materializes an omitted variant after the first prompt.
    f.remote.lock().unwrap().session["model"]["variant"] = json!("default");
    assert_eq!(adapter.agent().unwrap()["agent_status"], "working");
    f.finish(None, "stop");
    let report = adapter.reconcile().unwrap().unwrap();
    assert_eq!(report["phase"], "complete");
    assert_eq!(report["message_id"], id);
    assert_eq!(adapter.agent().unwrap()["interactive_ready"], true);
    assert_eq!(f.posts(), 1);
    assert_eq!(adapter.reconcile().unwrap().unwrap(), report);
    assert!(session::register(&f.root, &f.state).is_err());
}

#[test]
fn refuses_identity_model_version_busy_and_approval_mismatches() {
    let f = Fixture::new();
    let adapter = f.adapter();
    for status in [
        json!({"type":"busy"}),
        json!({"type":"retry"}),
        json!({"type":"unknown"}),
        json!({}),
    ] {
        f.remote.lock().unwrap().status = Some(status);
        assert!(adapter.prompt("must not submit").is_err());
    }
    f.remote.lock().unwrap().status = None;
    f.remote.lock().unwrap().permissions = json!([{"sessionID":"sesfixture"}]);
    assert_eq!(adapter.agent().unwrap()["agent_status"], "blocked");
    assert!(adapter.prompt("must not approve").is_err());
    f.remote.lock().unwrap().permissions = json!([]);
    f.remote.lock().unwrap().questions = json!([{"sessionID":"sesfixture"}]);
    assert!(adapter.prompt("must not answer").is_err());
    f.remote.lock().unwrap().questions = json!([]);
    for (field, value) in [
        ("id", json!("sesother")),
        ("directory", json!("/wrong")),
        ("model", json!({"providerID":"other","id":"model"})),
    ] {
        let before = f.remote.lock().unwrap().session.clone();
        f.remote.lock().unwrap().session[field] = value;
        assert!(adapter.prompt("wrong binding").is_err());
        f.remote.lock().unwrap().session = before;
    }
    f.remote.lock().unwrap().version = "2.0.32".into();
    assert!(adapter.agent().unwrap_err().message.contains("2.0"));
    assert_eq!(f.posts(), 0);
    assert!(receiver_factory(&json!({"adapter":"unsupported"})).is_err());
}

#[test]
fn uncertain_post_is_reconciled_without_duplicate_submission() {
    let f = Fixture::new();
    let adapter = f.adapter();
    f.remote.lock().unwrap().lose_response = true;
    assert!(adapter.prompt("one submission").is_err());
    assert_eq!(f.posts(), 1);
    assert!(adapter.prompt("never resend").is_err());
    assert_eq!(f.posts(), 1);
    f.finish(None, "stop");
    // Recreate the production adapter, as after a coordinator restart.
    assert_eq!(
        f.adapter().reconcile().unwrap().unwrap()["phase"],
        "complete"
    );
    assert_eq!(f.posts(), 1);
}

#[test]
fn absence_timeout_partial_failure_and_foreign_reply_remain_distinct() {
    let f = Fixture::new();
    let adapter = f.adapter();
    f.remote.lock().unwrap().reject_post = true;
    assert!(adapter.prompt("ambiguous failure").is_err());
    let mut journal = util::read_json(&f.root.join("submission.json")).unwrap();
    journal["deadline"] = json!(0);
    util::atomic_write_json(&f.root.join("submission.json"), &journal).unwrap();
    assert_eq!(
        adapter.reconcile().unwrap().unwrap()["phase"],
        "needs_attention"
    );
    assert!(adapter.prompt("no blind retry").is_err());
    assert_eq!(f.posts(), 1);
    let f = Fixture::new();
    let adapter = f.adapter();
    adapter.prompt("inspect").unwrap();
    f.finish(None, "stop");
    let index = f.remote.lock().unwrap().messages.len() - 1;
    f.remote.lock().unwrap().messages[index]["info"]["time"]["completed"] = Value::Null;
    assert_eq!(adapter.reconcile().unwrap().unwrap()["phase"], "running");
    f.remote.lock().unwrap().messages[index]["info"]["time"]["completed"] = json!(1);
    f.remote.lock().unwrap().messages[index]["info"]["providerID"] = json!("wrong");
    assert!(adapter.reconcile().is_err());
    f.remote.lock().unwrap().messages[index]["info"]["providerID"] = json!("provider");
    f.remote.lock().unwrap().messages[index]["info"]["error"] = json!({"name":"ProviderAuthError"});
    assert_eq!(adapter.reconcile().unwrap().unwrap()["phase"], "failed");
    let f = Fixture::new();
    f.adapter().prompt("inspect").unwrap();
    f.finish(None, "length");
    assert_eq!(
        f.adapter().reconcile().unwrap().unwrap()["phase"],
        "decision"
    );
}

fn healthy_watcher(root: &Path) -> util::FileLock {
    util::atomic_write_json(&root.join("watch.json"),&json!({"status":"running","pid":std::process::id(),
        "start_ticks":util::process_start_ticks(std::process::id()),"heartbeat_at":util::now(),"max_age":60})).unwrap();
    util::FileLock::acquire(&root.join("watch.lock"), false)
        .unwrap()
        .unwrap()
}

fn git(root: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().into()
}

fn managed_queue(f: &Fixture) -> (PathBuf, util::FileLock, util::FileLock) {
    git(&f.cwd, &["init", "-q"]);
    git(&f.cwd, &["config", "user.name", "Fixture"]);
    git(&f.cwd, &["config", "user.email", "fixture@example.invalid"]);
    fs::write(f.cwd.join("source"), "candidate").unwrap();
    git(&f.cwd, &["add", "source"]);
    git(&f.cwd, &["commit", "-qm", "candidate"]);
    let sha = git(&f.cwd, &["rev-parse", "HEAD"]);
    let q = f.root.parent().unwrap().join("queue");
    queue::init(&q, Some(&f.root), 30).unwrap();
    let guard_watch = healthy_watcher(&f.root);
    let queue_watch = healthy_watcher(&q);
    let mut state = queue::load_queue(&q).unwrap();
    queue::enable(&q, &mut state, &receiver_factory).unwrap();
    queue::add(&q,&json!({"id":"review","kind":"signal","task":"o/r#1","epoch":1,"assignment":"a1","candidate":sha,"owner":"reviewer",
        "reason":"review","condition":"report checked","timeout_seconds":600})).unwrap();
    let spec = f.root.join("task.json");
    util::atomic_write_json(
        &spec,
        &json!({"task":"o/r#1","wait":"review","goal":"review",
        "acceptance":[{"id":"reviewed","description":"findings checked"}],
        "authorization":[{"id":"review","action":"review","source":"test"}]}),
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_herdr-flow"))
        .args([
            "queue",
            "task-add",
            "--state-dir",
            q.to_str().unwrap(),
            "--spec",
            spec.to_str().unwrap(),
        ])
        .env("OPENCODE_SESSION_ID", "sesfixture")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    (q, guard_watch, queue_watch)
}

#[test]
fn production_factory_dispatch_collect_delivery_ack_and_application() {
    let f = Fixture::new();
    let (q, _guard_watch, _queue_watch) = managed_queue(&f);
    let readiness = runtime::readiness(&q, Some("sesfixture"), &receiver_factory).unwrap();
    assert_eq!(readiness["active_wakeup"], true);
    assert_eq!(readiness["safe_rotation"], false);
    assert_eq!(readiness["context_telemetry"], false);
    let prompt = f.root.join("prompt.txt");
    fs::write(&prompt, "Review only; report findings.").unwrap();
    let spec = json!({"queue_dir":q,"wait":"review","prompt_file":prompt});
    let submitted = session::dispatch(&f.root, &spec).unwrap();
    assert!(session::dispatch(&f.root, &spec).is_err());
    assert_eq!(f.posts(), 1);
    f.finish(None, "stop");
    assert_eq!(session::collect(&f.root).unwrap()["phase"], "signalled");
    assert_eq!(session::collect(&f.root).unwrap()["phase"], "signalled");
    let state = queue::tick(&q, &receiver_factory, None).unwrap();
    assert_eq!(f.posts(), 2);
    let delivery = state["active"].as_str().unwrap();
    let event = state["deliveries"][delivery]["events"][0]
        .as_str()
        .unwrap()
        .to_string();
    let mut state = queue::acknowledge(&q, delivery, "sesfixture").unwrap();
    let version = state["tasks"]["o/r#1"]["state_version"].clone();
    progress::apply(
        &mut state,
        &json!({"task":"o/r#1","state_version":version,"event":event,
        "acceptance":{"reviewed":f.root.join("submissions")},"next_action":{"kind":"complete"}}),
        util::now(),
    )
    .unwrap();
    assert_eq!(state["tasks"]["o/r#1"]["phase"], "completed");
    queue::save(&q, &mut state).unwrap();
    let retained = util::read_json(&f.root.join("submissions").join(format!(
        "{}.json",
        submitted["message_id"].as_str().unwrap()
    )))
    .unwrap();
    assert_eq!(retained["phase"], "signalled");
    assert!(guard::scope(&f.state).get("opencode").is_some());
}

#[test]
fn rejects_stale_candidate_and_uses_opencode_receiver_environment() {
    let f = Fixture::new();
    let (q, _guard_watch, _queue_watch) = managed_queue(&f);
    let prompt = f.root.join("prompt.txt");
    fs::write(&prompt, "review").unwrap();
    session::dispatch(
        &f.root,
        &json!({"queue_dir":q,"wait":"review","prompt_file":prompt}),
    )
    .unwrap();
    f.finish(None, "stop");
    let mut state = queue::load_queue(&q).unwrap();
    state["waits"]["review"]["candidate"] = json!("b".repeat(40));
    queue::save(&q, &mut state).unwrap();
    assert!(session::collect(&f.root).is_err());
    assert!(!q.join("results/review.json").exists());
    let output = Command::new(env!("CARGO_BIN_EXE_herdr-flow"))
        .args(["queue", "readiness", "--state-dir", q.to_str().unwrap()])
        .env("OPENCODE_SESSION_ID", "sesfixture")
        .env("CODEX_THREAD_ID", "unrelated-codex-thread")
        .output()
        .unwrap();
    let report: J = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["session"], "sesfixture");
    let output = Command::new(env!("CARGO_BIN_EXE_herdr-flow"))
        .args([
            "queue",
            "ack",
            "--state-dir",
            q.to_str().unwrap(),
            "--delivery",
            "missing",
        ])
        .env("OPENCODE_SESSION_ID", "seswrong")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("session mismatch"));
}

#[test]
fn rejects_remote_endpoint_and_non_session_paths() {
    let f = Fixture::new();
    for endpoint in [
        "http://example.com:80",
        "http://127.0.0.1:4096/extra",
        "http://user@127.0.0.1:4096",
        "http://127.0.0.1:4096/?secret=x",
    ] {
        let mut state = f.state.clone();
        state["endpoint"] = json!(endpoint);
        assert!(SessionAdapter::new(&state).is_err());
    }
    let mut state = f.state.clone();
    state["session"] = json!("ses/../../other");
    assert!(SessionAdapter::new(&state).is_err());
}

#[test]
fn preserves_explicit_model_variant_and_rejects_changed_reply_variant() {
    let mut f = Fixture::new();
    f.state["model"]["variant"] = json!("high");
    f.remote.lock().unwrap().session["model"]["variant"] = json!("high");
    let adapter = f.adapter();
    adapter.prompt("preserve settings").unwrap();
    assert_eq!(f.remote.lock().unwrap().posts[0]["variant"], "high");
    f.finish(None, "stop");
    let index = f.remote.lock().unwrap().messages.len() - 1;
    f.remote.lock().unwrap().messages[index]["info"]["variant"] = json!("low");
    assert!(adapter.reconcile().is_err());
    f.remote.lock().unwrap().messages[index]["info"]["variant"] = json!("high");
    assert_eq!(adapter.reconcile().unwrap().unwrap()["phase"], "complete");
}

fn cli(root: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_herdr-flow"))
        .arg("opencode")
        .args(args)
        .args(["--state-dir", root.to_str().unwrap()])
        .output()
        .unwrap()
}

#[test]
fn cli_registration_status_watch_contention_and_errors() {
    let f = Fixture::new();
    let root = f.root.parent().unwrap().join("cli-state");
    let spec = f.root.join("registration.json");
    util::atomic_write_json(&spec, &f.state).unwrap();
    let out = cli(&root, &["register", "--spec", spec.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = cli(&root, &["status"]);
    assert!(out.status.success());
    let state: J = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(state["interactive_ready"], true);
    let lock = guard::locked(&root).unwrap();
    assert!(cli(&root, &["watch", "--once"]).status.success());
    drop(lock);
    let lock = util::FileLock::acquire(&root.join("submission.lock"), false)
        .unwrap()
        .unwrap();
    assert!(cli(&root, &["watch", "--once"]).status.success());
    drop(lock);
    assert!(cli(&root, &["watch", "--once"]).status.success());
    assert_eq!(
        util::read_json(&root.join("state.json")).unwrap()["observation"]["native_status"],
        "idle"
    );
    for args in [
        vec!["collect"],
        vec!["dispatch"],
        vec!["watch", "--once", "--background"],
        vec!["watch", "--ready-fd", "invalid"],
        vec!["status", "--unknown", "value"],
    ] {
        assert!(!cli(&root, &args).status.success(), "{args:?}");
    }
    assert!(cli(&root, &["status", "--help"]).status.success());
}
