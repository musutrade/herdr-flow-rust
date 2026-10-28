use super::*;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::{
    io::{BufRead, BufReader, Write},
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

#[path = "stage_retirement_broker.rs"]
mod real_broker;

#[path = "stage_retirement_tui.rs"]
mod real_tui;

struct Frontend(Child);

impl Frontend {
    fn new() -> Self {
        Self(Command::new("sleep").arg("60").spawn().unwrap())
    }
    fn stop(&mut self) {
        self.0.kill().unwrap();
        self.0.wait().unwrap();
    }
}

impl Drop for Frontend {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Only the line-delimited local Herdr protocol is exercised here. The close
/// updates pane visibility but deliberately does not stop the frontend child.
struct PresentationServer {
    socket: J,
    calls: Arc<Mutex<Vec<J>>>,
    stopped: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl PresentationServer {
    fn new(path: &Path, activate: Option<Arc<AtomicBool>>) -> Self {
        let listener = UnixListener::bind(path).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let calls = Arc::new(Mutex::new(vec![]));
        let stopped = Arc::new(AtomicBool::new(false));
        let (recorded, stop) = (calls.clone(), stopped.clone());
        let thread = std::thread::spawn(move || {
            let mut present = true;
            while !stop.load(Ordering::SeqCst) {
                let (mut stream, _) = match listener.accept() {
                    Ok(value) => value,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(1));
                        continue;
                    }
                    Err(error) => panic!("Herdr fixture accept: {error}"),
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut line = String::new();
                BufReader::new(stream.try_clone().unwrap())
                    .read_line(&mut line)
                    .unwrap();
                let request: J = serde_json::from_str(&line).unwrap();
                assert_eq!(request["params"]["pane_id"], "owned-pane");
                recorded.lock().unwrap().push(request.clone());
                let mut reply = presentation_reply(&request, &mut present, activate.as_ref());
                reply["id"] = request["id"].clone();
                writeln!(stream, "{reply}").unwrap();
            }
        });
        Self {
            socket: upstream::socket_identity(path).unwrap(),
            calls,
            stopped,
            thread: Some(thread),
        }
    }
}

fn presentation_reply(request: &J, present: &mut bool, activate: Option<&Arc<AtomicBool>>) -> J {
    match request["method"].as_str().unwrap() {
        "pane.get" if *present => json!({"result":{"pane":{
            "pane_id":"owned-pane","terminal_id":"owned-terminal",
            "agent_session":{"value":WORKER}}}}),
        "pane.get" => json!({"error":{"code":"pane_not_found","message":"closed"}}),
        "pane.process_info" => json!({"result":{"process_info":{
            "shell_pid":std::process::id(),"foreground_processes":[]}}}),
        "pane.close" => {
            *present = false;
            if let Some(active) = activate {
                active.store(true, Ordering::SeqCst);
            }
            json!({"result":{}})
        }
        method => panic!("unexpected Herdr fixture method: {method}"),
    }
}

impl Drop for PresentationServer {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::SeqCst);
        self.thread.take().unwrap().join().unwrap();
    }
}

fn install_presentation(
    fixture: &Fixture,
    server: &PresentationServer,
    frontend: &Frontend,
) -> PathBuf {
    let mut env = fixture.active();
    let root = fixture.root();
    let mut runtime = read(&root, "runtime.json").unwrap();
    runtime["native_tui"] = json!({"binary":"/bin/true","herdr_socket":server.socket});
    runtime["model"] = json!({"id":"selected","providerID":"existing","reasoning_effort":"high"});
    util::atomic_write_json(&root.join("runtime.json"), &runtime).unwrap();
    let mut journal = read(&root, "upstream-session.json").unwrap();
    journal["runtime"] = runtime.clone();
    util::atomic_write_json(&root.join("upstream-session.json"), &journal).unwrap();
    let directory = root.join("tui-attachment");
    util::ensure_private_dir(&directory, true).unwrap();
    let record = json!({"phase":"ready","nonce":"original-attachment","instance":env.instance_id,
        "runtime":root,"session":WORKER,"herdr_socket":server.socket,"directory":directory,
        "pane":"owned-pane","terminal":"owned-terminal","cwd":fixture.clone_path()});
    let ticks = task_env::process_start_time(frontend.0.id() as i32).unwrap();
    let marker = json!({"status":"ready","nonce":record["nonce"],"session":WORKER,
        "binary":"/bin/true","server_socket":runtime["upstream_socket"]["path"],
        "pid":frontend.0.id(),"pid_start_ticks":ticks,"wrapper_pid":frontend.0.id(),"wrapper_start_ticks":ticks,
        "runtime":{"cwd":runtime["cwd"],"model":"selected","modelProvider":"existing","reasoningEffort":"high"}});
    util::atomic_write_json(&directory.join("tui.json"), &marker).unwrap();
    util::atomic_write_json(&root.join("presentation.json"), &record).unwrap();
    // Capture through the real marker/binding validator. Only fixture setup
    // changes its initial snapshot; every drive below uses the durable plan.
    let (presentation, frontends) = tui::retirement_snapshot(&env, &root).unwrap();
    let mut plan = load(&env).unwrap();
    plan.entries[0].runtime = runtime;
    plan.entries[0].sessions[0] = journal;
    plan.entries[0].presentation = presentation;
    plan.entries[0].frontends = frontends;
    save(&mut env, &plan).unwrap();
    drop(env);
    // The older pane/process fixtures install the production policy's drain
    // capability; real_tui below exercises its actual buffered socket traffic.
    tui::test_drain_handshake(&record, &read(&root, "runtime.json").unwrap());
    directory
}

fn assert_original_resources(fixture: &mut Fixture) {
    assert!(fixture.clone_path().join("evidence").is_file());
    assert!(fixture.broker.try_wait().unwrap().is_none());
    assert!(fixture.unrelated.try_wait().unwrap().is_none());
    assert!(!fixture.root().join(MARKER).exists());
    assert_eq!(fixture.plan().entries[0].phase, "planned");
}

fn uncertain_delete_server(calls: Arc<Mutex<Vec<J>>>) -> rpc_server::Server {
    let mut deletes = 0;
    rpc_server::Server::start_connections(6, move |mut peer| {
        peer.initialize();
        let request = peer.request();
        calls.lock().unwrap().push(request.clone());
        match request["method"].as_str().unwrap() {
            "thread/read" => {
                assert_eq!(request["params"]["threadId"], WORKER);
                let cwd = calls.lock().unwrap()[0]["fixture_cwd"].clone();
                peer.json(&json!({"id":request["id"],"result":{"thread":{
                    "id":WORKER,"cwd":cwd,"status":{"type":"idle","activeFlags":[]}}}}));
            }
            "thread/delete" => {
                assert_eq!(request["params"]["threadId"], WORKER);
                deletes += 1;
                if deletes == 1 {
                    // Deletion reached the server, but its acknowledgement was
                    // lost. The client must retain the durable deleting intent.
                    return;
                }
                assert_eq!(deletes, 2);
                peer.json(&json!({"id":request["id"],"error":{
                    "code":-32600,"message":format!("thread not found: {WORKER}")}}));
            }
            "thread/list" => {
                peer.json(&json!({"id":request["id"],"result":{"data":[],"nextCursor":null}}));
                let archived = peer.request();
                calls.lock().unwrap().push(archived.clone());
                assert_eq!(archived["method"], "thread/list");
                assert_eq!(archived["params"]["archived"], true);
                peer.json(&json!({"id":archived["id"],"result":{"data":[],"nextCursor":null}}));
            }
            method => panic!("unexpected RPC after fault: {method}"),
        }
    })
}

#[test]
fn uncertain_delete_keeps_durable_intent_and_exact_not_found_retry_completes() {
    let calls = Arc::new(Mutex::new(vec![json!({})]));
    let service = uncertain_delete_server(calls.clone());
    let mut fixture = Fixture::new(&service.path);
    calls.lock().unwrap()[0]["fixture_cwd"] = json!(fixture.clone_path());
    let original_result = fixture.state()["events"]["result"].clone();
    fixture.pending_tick();
    fixture.pending_tick();
    assert!(drive_owner(&fixture.owner).is_err());
    let uncertain = fixture.plan();
    assert_eq!(uncertain.entries[0].phase, "sessions");
    assert_eq!(uncertain.entries[0].sessions[0]["phase"], "deleting");
    assert_eq!(uncertain.entries[0].sessions[0]["session"], WORKER);
    assert_eq!(
        read(&fixture.root(), "upstream-session.json").unwrap()["phase"],
        "created"
    );
    assert!(fixture.clone_path().is_dir());
    assert!(fixture.broker.try_wait().unwrap().is_some());
    assert!(fixture.unrelated.try_wait().unwrap().is_none());
    // A new restore reloads the deleting checkpoint and retries exactly that
    // session. It neither asks for another original result nor creates a turn.
    fixture.pending_tick();
    assert_eq!(fixture.plan().entries[0].sessions[0]["phase"], "deleted");
    assert_eq!(
        read(&fixture.root(), "upstream-session.json").unwrap()["phase"],
        "deleted"
    );
    drive_owner(&fixture.owner).unwrap();
    service.finish();
    assert_eq!(fixture.plan().phase, "complete");
    assert!(fixture.clone_path().join("evidence").is_file());
    assert!(fixture.unrelated.try_wait().unwrap().is_none());
    assert_eq!(fixture.state()["events"]["result"], original_result);
    let calls = calls.lock().unwrap();
    let methods: Vec<_> = calls
        .iter()
        .skip(1)
        .map(|v| v["method"].as_str().unwrap())
        .collect();
    assert_eq!(
        methods,
        [
            "thread/read",
            "thread/read",
            "thread/read",
            "thread/delete",
            "thread/delete",
            "thread/list",
            "thread/list"
        ]
    );
}

#[test]
fn runtime_and_socket_replacements_are_rejected_before_any_native_effect() {
    for replace_socket in [false, true] {
        let service = server(0, false, Arc::new(Mutex::new(vec![])));
        let mut fixture = Fixture::new(&service.path);
        if replace_socket {
            let original_socket = service.path.with_file_name("original-socket");
            fs::rename(&service.path, &original_socket).unwrap();
            let _replacement = UnixListener::bind(&service.path).unwrap();
            fs::set_permissions(&service.path, fs::Permissions::from_mode(0o600)).unwrap();
            let error = drive_owner(&fixture.owner).unwrap_err();
            assert!(error.message.contains("socket identity changed"), "{error}");
            assert!(original_socket.exists());
            assert!(service.path.exists());
        } else {
            let mut replacement = read(&fixture.root(), "runtime.json").unwrap();
            replacement["instance"] = json!("foreign-runtime");
            util::atomic_write_json(&fixture.root().join("runtime.json"), &replacement).unwrap();
            let error = drive_owner(&fixture.owner).unwrap_err();
            assert!(
                error.message.contains("frozen retirement runtime changed"),
                "{error}"
            );
            assert_eq!(read(&fixture.root(), "runtime.json").unwrap(), replacement);
        }
        assert_original_resources(&mut fixture);
        service.finish();
    }
}

#[test]
fn stage_completion_preserves_original_review_registration_and_replaced_directory() {
    let calls = Arc::new(Mutex::new(vec![json!({})]));
    let service = server(5, false, calls.clone());
    let mut fixture = Fixture::new(&service.path);
    calls.lock().unwrap()[0]["fixture_cwd"] = json!(fixture.clone_path());
    for _ in 0..3 {
        fixture.pending_tick();
    }
    let registration = fixture.active().review_workspaces[ASSIGNMENT].clone();
    let original = fixture.clone_path().with_file_name("original-clone");
    fs::rename(fixture.clone_path(), &original).unwrap();
    util::ensure_private_dir(&fixture.clone_path(), true).unwrap();
    fs::write(fixture.clone_path().join("foreign"), "preserve").unwrap();
    drive_owner(&fixture.owner).unwrap();
    service.finish();
    assert_eq!(fixture.plan().phase, "complete");
    assert_eq!(fixture.active().review_workspaces[ASSIGNMENT], registration);
    assert_eq!(
        fs::read_to_string(fixture.clone_path().join("foreign")).unwrap(),
        "preserve"
    );
    assert_eq!(
        fs::read_to_string(original.join("evidence")).unwrap(),
        "old candidate"
    );
    assert!(fixture.unrelated.try_wait().unwrap().is_none());
}

#[test]
fn stage_completion_keeps_new_queue_and_runtime_clone_references_and_sources() {
    for runtime_reference in [false, true] {
        let calls = Arc::new(Mutex::new(vec![json!({})]));
        let service = server(5, false, calls.clone());
        let mut fixture = Fixture::new(&service.path);
        calls.lock().unwrap()[0]["fixture_cwd"] = json!(fixture.clone_path());
        for _ in 0..3 {
            fixture.pending_tick();
        }
        let registration = fixture.active().review_workspaces[ASSIGNMENT].clone();
        let reference_root = fixture.temp.path().join("scratch/rt/later-worker");
        if runtime_reference {
            util::ensure_private_dir(&reference_root, true).unwrap();
            util::atomic_write_json(
                &reference_root.join("creation.json"),
                &json!({"phase":"unknown","packet":{"cwd":fixture.clone_path()}}),
            )
            .unwrap();
        } else {
            let _lock = queue::locked(&fixture.owner.queue_root, false).unwrap();
            let mut state = fixture.state();
            state["new_inspection"] = json!({"candidate_workspace":fixture.clone_path()});
            queue::save(&fixture.owner.queue_root, &mut state).unwrap();
        }
        drive_owner(&fixture.owner).unwrap();
        service.finish();
        assert_eq!(fixture.plan().phase, "complete");
        assert_eq!(fixture.active().review_workspaces[ASSIGNMENT], registration);
        assert!(fixture.clone_path().join("evidence").is_file());
        assert!(fixture.unrelated.try_wait().unwrap().is_none());
        if runtime_reference {
            assert_eq!(
                read(&reference_root, "creation.json").unwrap()["packet"]["cwd"],
                json!(fixture.clone_path())
            );
        } else {
            assert_eq!(
                fixture.state()["new_inspection"]["candidate_workspace"],
                json!(fixture.clone_path())
            );
        }
    }
}

#[test]
fn held_owner_shared_permit_prevents_all_effects_and_release_allows_continuation() {
    let calls = Arc::new(Mutex::new(vec![json!({})]));
    let service = server(5, false, calls.clone());
    let mut fixture = Fixture::new(&service.path);
    calls.lock().unwrap()[0]["fixture_cwd"] = json!(fixture.clone_path());
    let environment = fixture.active().state_file().to_path_buf();
    let authority = util::protected::read_path(&environment).unwrap();
    let before = fs::read(&authority).unwrap();
    let shared = lifecycle::write_permit(&fixture.owner).unwrap();
    assert_eq!(
        drive_owner(&fixture.owner).unwrap_err().kind,
        crate::ErrorKind::StateBusy
    );
    drive(&fixture.owner.queue_root).unwrap();
    assert_eq!(fs::read(&authority).unwrap(), before);
    assert_eq!(calls.lock().unwrap().len(), 1);
    assert!(fixture.broker.try_wait().unwrap().is_none());
    assert!(fixture.unrelated.try_wait().unwrap().is_none());
    assert!(fixture.clone_path().join("evidence").is_file());
    assert!(!fixture.root().join(MARKER).exists());
    drop(shared);
    for _ in 0..3 {
        fixture.pending_tick();
    }
    drive_owner(&fixture.owner).unwrap();
    service.finish();
    assert_eq!(fixture.plan().phase, "complete");
    assert!(fixture.clone_path().join("evidence").is_file());
    assert!(fixture.unrelated.try_wait().unwrap().is_none());
}

#[test]
fn closed_owned_pane_keeps_live_frontend_and_native_resources_until_frontend_exit_and_idle_recheck()
{
    let calls = Arc::new(Mutex::new(vec![json!({})]));
    let service = server(7, false, calls.clone());
    let mut fixture = Fixture::new(&service.path);
    calls.lock().unwrap()[0]["fixture_cwd"] = json!(fixture.clone_path());
    let herdr = PresentationServer::new(&fixture.temp.path().join("herdr.sock"), None);
    let mut frontend = Frontend::new();
    let directory = install_presentation(&fixture, &herdr, &frontend);
    fixture.pending_tick();
    let error = drive_owner(&fixture.owner).unwrap_err();
    assert!(
        error.message.contains("frontend is still running"),
        "{error}"
    );
    let plan = fixture.plan();
    assert_eq!(plan.entries[0].phase, "retiring");
    assert_eq!(
        plan.entries[0].presentation.as_ref().unwrap()["phase"],
        "closed"
    );
    assert_eq!(plan.entries[0].sessions[0]["phase"], "created");
    assert!(frontend.0.try_wait().unwrap().is_none());
    assert!(fixture.broker.try_wait().unwrap().is_none());
    assert!(fixture.clone_path().join("evidence").is_file());
    assert!(directory.join("tui.json").is_file());
    assert_eq!(
        calls.lock().unwrap().len(),
        3,
        "native idle was checked before closing the frontend"
    );
    let methods: Vec<_> = herdr
        .calls
        .lock()
        .unwrap()
        .iter()
        .map(|v| v["method"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        methods,
        ["pane.get", "pane.process_info", "pane.close", "pane.get"]
    );
    frontend.stop();
    fixture.pending_tick();
    assert_eq!(fixture.plan().entries[0].phase, "sessions");
    let native = calls.lock().unwrap();
    assert_eq!(native.len(), 5);
    assert_eq!(native[4]["method"], "thread/read");
    assert_eq!(native[4]["params"]["threadId"], WORKER);
    drop(native);
    fixture.pending_tick();
    drive_owner(&fixture.owner).unwrap();
    service.finish();
    assert_eq!(fixture.plan().phase, "complete");
    assert!(fixture.clone_path().join("evidence").is_file());
    assert!(fixture.unrelated.try_wait().unwrap().is_none());
    assert_eq!(
        herdr
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|v| v["method"] == "pane.close")
            .count(),
        1
    );
}

#[test]
fn upstream_becoming_active_during_owned_pane_close_retains_session_broker_and_clone() {
    let active = Arc::new(AtomicBool::new(false));
    let calls = Arc::new(Mutex::new(vec![json!({})]));
    let (observed, status) = (calls.clone(), active.clone());
    let service = rpc_server::Server::start_connections(3, move |mut peer| {
        peer.initialize();
        let request = peer.request();
        assert_eq!(request["method"], "thread/read");
        assert_eq!(request["params"]["threadId"], WORKER);
        observed.lock().unwrap().push(request.clone());
        let cwd = observed.lock().unwrap()[0]["fixture_cwd"].clone();
        peer.json(&json!({"id":request["id"],"result":{"thread":{"id":WORKER,"cwd":cwd,
            "status":{"type":if status.load(Ordering::SeqCst) {"active"} else {"idle"},"activeFlags":[]}}}}));
    });
    let mut fixture = Fixture::new(&service.path);
    calls.lock().unwrap()[0]["fixture_cwd"] = json!(fixture.clone_path());
    let herdr = PresentationServer::new(
        &fixture.temp.path().join("herdr.sock"),
        Some(active.clone()),
    );
    let mut frontend = Frontend::new();
    install_presentation(&fixture, &herdr, &frontend);
    fixture.pending_tick();
    frontend.stop();
    let error = drive_owner(&fixture.owner).unwrap_err();
    assert!(error.message.contains("current native work"), "{error}");
    service.finish();
    assert!(active.load(Ordering::SeqCst));
    let plan = fixture.plan();
    assert_eq!(plan.entries[0].phase, "retiring");
    assert_eq!(
        plan.entries[0].presentation.as_ref().unwrap()["phase"],
        "closed"
    );
    assert_eq!(plan.entries[0].sessions[0]["phase"], "created");
    assert!(fixture.broker.try_wait().unwrap().is_none());
    assert!(fixture.unrelated.try_wait().unwrap().is_none());
    assert!(fixture.clone_path().join("evidence").is_file());
    assert_eq!(calls.lock().unwrap().len(), 4);
    assert_eq!(
        herdr
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|v| v["method"] == "pane.close")
            .count(),
        1
    );
}

#[test]
fn changed_frontend_marker_pid_or_nonce_is_preserved_before_any_close_or_native_operation() {
    for change_pid in [false, true] {
        let calls = Arc::new(Mutex::new(vec![]));
        let service = server(0, false, calls.clone());
        let mut fixture = Fixture::new(&service.path);
        let herdr = PresentationServer::new(&fixture.temp.path().join("herdr.sock"), None);
        let mut frontend = Frontend::new();
        let directory = install_presentation(&fixture, &herdr, &frontend);
        let mut marker = read(&directory, "tui.json").unwrap();
        if change_pid {
            marker["pid"] = json!(fixture.unrelated.id());
            marker["pid_start_ticks"] =
                json!(task_env::process_start_time(fixture.unrelated.id() as i32).unwrap());
        } else {
            marker["nonce"] = json!("replacement-attachment");
        }
        util::atomic_write_json(&directory.join("tui.json"), &marker).unwrap();
        let before = fs::read(directory.join("tui.json")).unwrap();
        let error = drive_owner(&fixture.owner).unwrap_err();
        let expected = if change_pid {
            "frontend process identities changed"
        } else {
            "session/socket changed"
        };
        assert!(error.message.contains(expected), "{error}");
        assert_eq!(fs::read(directory.join("tui.json")).unwrap(), before);
        assert!(herdr.calls.lock().unwrap().is_empty());
        assert!(calls.lock().unwrap().is_empty());
        assert!(frontend.0.try_wait().unwrap().is_none());
        assert_original_resources(&mut fixture);
        service.finish();
    }
}
