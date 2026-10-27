//! Owned runtime boundary tests; live tests require installed native binaries.
use herdr_flow::{delivery, model::J, progress, queue, task_env::*, task_runtime};
use serde_json::json;
use std::os::unix::fs::PermissionsExt;
use std::{fs, path::Path, process::Command};
static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
struct OwnedEnvironment {
    env: Option<TaskEnvironment>,
    cleanup_on_drop: bool,
}
impl std::ops::Deref for OwnedEnvironment {
    type Target = TaskEnvironment;
    fn deref(&self) -> &TaskEnvironment {
        self.env.as_ref().unwrap()
    }
}
impl std::ops::DerefMut for OwnedEnvironment {
    fn deref_mut(&mut self) -> &mut TaskEnvironment {
        self.env.as_mut().unwrap()
    }
}
impl Drop for OwnedEnvironment {
    fn drop(&mut self) {
        if !self.cleanup_on_drop {
            return;
        }
        if let Some(env) = self.env.as_ref() {
            if is_registered(&env.task_id).unwrap_or(false) {
                if let Err(error) = destroy_environment(env) {
                    eprintln!("test cleanup failed: {error}");
                }
            }
        }
    }
}
impl OwnedEnvironment {
    // Test-only custody transfer: release the inner lock, suppress this
    // wrapper's destructive Drop, and arm a guard that can finish a confirmed
    // delivery or resume its cleanup plan if the handoff fails.
    fn into_handoff(mut self) -> HandoffGuard {
        self.cleanup_on_drop = false;
        let env = self.env.take().expect("live environment handle");
        let task_id = env.task_id.clone();
        env.release_ownership();
        HandoffGuard {
            task_id,
            armed: true,
        }
    }
}
struct HandoffGuard {
    task_id: String,
    armed: bool,
}
impl HandoffGuard {
    fn cleanup(&mut self) {
        if !self.armed {
            return;
        }
        self.armed = false;
        let registered = match is_registered(&self.task_id) {
            Ok(registered) => registered,
            Err(error) => {
                eprintln!("handoff cleanup registry read failed: {error}");
                false
            }
        };
        let plan_before = match cleanup_state_path(&self.task_id) {
            Ok(path) => path.exists(),
            Err(error) => {
                eprintln!("handoff cleanup plan path failed: {error}");
                false
            }
        };
        if registered {
            match restore_environment(&self.task_id) {
                Ok(mut env) => {
                    let phase = env.delivery["phase"].as_str().unwrap_or_default();
                    if phase == "confirmed" || phase == "cleaning" {
                        if let Err(error) = delivery::finish(&mut env) {
                            // Finish owns the confirmed/cleaning cleanup plan.
                            // Do not erase its journal or roots with a blind destroy.
                            eprintln!(
                                "handoff cleanup finish failed; preserving state/plan: {error}"
                            );
                        }
                    } else {
                        eprintln!(
                            "handoff cleanup preserving non-confirmed phase {phase:?}; no valid delivery finish"
                        );
                    }
                }
                Err(error) => {
                    eprintln!("handoff cleanup could not restore environment: {error}");
                }
            }
        }
        let plan_after = match cleanup_state_path(&self.task_id) {
            Ok(path) => path.exists(),
            Err(error) => {
                eprintln!("handoff cleanup plan recheck failed: {error}");
                plan_before
            }
        };
        if plan_after {
            if let Err(error) = resume_delivery_cleanup(&self.task_id) {
                eprintln!("handoff cleanup plan resume failed; retained: {error}");
            }
        }
    }
}
impl Drop for HandoffGuard {
    fn drop(&mut self) {
        self.cleanup();
    }
}
fn environment_with(root: &Path, task_id: &str, targets: Vec<DeliveryTarget>) -> OwnedEnvironment {
    std::env::set_var("HERDR_TASK_ENV_ROOT", root.join("registry"));
    OwnedEnvironment {
        env: Some(
            create_environment(
                task_id,
                &root.join("workspace"),
                targets,
                &root.join("temporary"),
            )
            .unwrap(),
        ),
        cleanup_on_drop: true,
    }
}
fn environment(root: &Path) -> OwnedEnvironment {
    environment_with(root, "runtime-test", vec![])
}
fn temp() -> tempfile::TempDir {
    tempfile::Builder::new()
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir()
        .unwrap()
}
fn git(root: &Path, args: &[&str]) -> String {
    let result = Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    String::from_utf8(result.stdout).unwrap().trim().into()
}
fn setup(env: &TaskEnvironment, role: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let q = env.temp_root.join("queue");
    queue::init(&q, None, 30).unwrap();
    setup_in_queue(env, role, &q)
}
fn setup_in_queue(
    env: &TaskEnvironment,
    role: &str,
    q: &Path,
) -> (std::path::PathBuf, std::path::PathBuf) {
    setup_in_queue_inner(env, role, q)
}
fn setup_in_queue_inner(
    env: &TaskEnvironment,
    role: &str,
    q: &Path,
) -> (std::path::PathBuf, std::path::PathBuf) {
    let source = env.workspace_root.join("source");
    common::create_dir_all(&source).unwrap();
    git(&source, &["init", "-q"]);
    git(&source, &["config", "user.name", "Test"]);
    git(&source, &["config", "user.email", "test@example.invalid"]);
    fs::write(source.join("file"), "version A").unwrap();
    git(&source, &["add", "file"]);
    git(&source, &["commit", "-qm", "A"]);
    let sha = git(&source, &["rev-parse", "HEAD"]);
    let cwd = env.workspace_root.join(if role == "development" {
        "development"
    } else {
        "acceptance"
    });
    git(
        &env.workspace_root,
        &[
            "clone",
            "-q",
            source.to_str().unwrap(),
            cwd.to_str().unwrap(),
        ],
    );
    let now = herdr_flow::util::now();
    let (wait_owner, wait_reason) = if role == "development" {
        ("developer", "development")
    } else {
        ("reviewer", "review")
    };
    let mut state = queue::load_queue(q).unwrap();
    let original = vec![json!(if role == "development" {
        "change file to version B and commit locally"
    } else {
        "file must contain version A"
    })];
    let plan = json!({"kind":"plan","task":"project","goal":"verify file",
        "basis":{"original":original,"changes":[],"specifications":[],"endpoint":"user delivery","confirmation":"user"},
        "authorization":[{"id":"verify","action":"review","source":"user"}]});
    progress::lineage::action(&mut state, &plan, now).unwrap();
    if let Some(target) = env.delivery_targets.first() {
        state["tasks"]["project"]["requirements"]["1"]["authorization"]["deliver"] =
            json!({"action":"delivery","source":"explicit disposable native delivery test"});
        state["tasks"]["project"]["requirements"]["1"]["basis"]["endpoint"] = json!(target.path);
    }
    if role == "development" {
        state["tasks"]["project"]["requirements"]["1"]["authorization"]["develop"] =
            json!({"action":"development","source":"explicit disposable native development test"});
    }
    let activity = json!({"kind":"activity","activity":"review","role":role,"input":sha,"session_epoch":1,
        "result_wait":{"id":"review","kind":"signal","task":"review","epoch":1,"assignment":"review-a1","owner":wait_owner,"reason":wait_reason,"condition":"report","timeout_seconds":600}});
    for spec in [
        json!({"kind":"artifact","artifact":{"id":sha,"adapter":"git","location":source}}),
        json!({"kind":"candidate","candidate":sha,"reason":"review"}),
        activity,
    ] {
        let mut spec = spec;
        spec["task"] = json!("project");
        spec["state_version"] = state["tasks"]["project"]["state_version"].clone();
        progress::lineage::action(&mut state, &spec, now).unwrap();
    }
    queue::save(q, &mut state).unwrap();
    (q.to_path_buf(), cwd)
}

#[test]
fn handoff_guard_resumes_a_plan_after_registry_removal() {
    let _lock = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let temp = temp();
    let env = environment(temp.path());
    let workspace = env.workspace_root.clone();
    let temporary = env.temp_root.clone();
    let plan = json!({
        "task_id":env.task_id,
        "instance_id":env.instance_id,
        "delivery_confirmed":true,
        "roots":[],
        "pending_processes":[],
        "pending_worktrees":[],
        "pending_files":[],
        "pending_root_removals":[],
        "quarantine":null,
        "started_at":0.0,
        "updated_at":0.0,
        "attempts":0,
        "last_error":null
    });
    let plan_path = cleanup_state_path(&env.task_id).unwrap();
    herdr_flow::util::atomic_write_json(&plan_path, &plan).unwrap();
    unregister_environment(&env.task_id, &env.instance_id).unwrap();
    fs::remove_file(env.state_file()).unwrap();
    let mut guard = env.into_handoff();
    guard.cleanup();
    assert!(!plan_path.exists());
    assert!(!is_registered("runtime-test").unwrap());
    assert_eq!(occupied_slots().unwrap(), 0);
    let _ = fs::remove_dir_all(workspace);
    let _ = fs::remove_dir_all(temporary);
}

#[test]
fn native_launch_does_not_silently_replace_upstream_authentication() {
    let _lock = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let temp = temp();
    let mut env = environment(temp.path());
    let root = task_runtime::prepare(&env, "configuration-policy").unwrap();
    let spec = json!({"adapter":"codex","binary":"/bin/true",
        "cwd":env.workspace_root,"port":1,"model":{"providerID":"fixture","id":"fixture"}});
    let outcome = task_runtime::launch(&mut env, &root, &spec);
    let rejected = match outcome {
        Err(error) => error.message.contains("upstream"),
        Ok(mut child) => {
            child.wait().unwrap();
            false
        }
    };
    let journal_exists = root.join("runtime.json").exists();
    destroy_environment(&env).unwrap();
    assert!(
        rejected,
        "native launch must not silently use a fresh credential home"
    );
    assert!(
        !journal_exists,
        "unsupported launch must not consume its creation attempt"
    );
}

#[test]
fn rejected_configuration_preserves_user_assets_and_creates_no_runtime_state() {
    let _lock = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let temp = temp();
    let mut env = environment(temp.path());
    let root = task_runtime::prepare(&env, "rejected-configuration").unwrap();
    let upstream = temp.path().join("upstream");
    fs::create_dir(&upstream).unwrap();
    for name in ["auth.json", "config.toml", "shared.db"] {
        fs::write(upstream.join(name), "synthetic-user-asset").unwrap();
    }
    for adapter in ["codex", "opencode"] {
        for mode in [
            json!("upstream"),
            J::Null,
            json!({"token":"synthetic-secret"}),
        ] {
            let spec = json!({"adapter":adapter,"configuration":mode,
                "binary":"/bin/true","cwd":env.workspace_root,"port":1});
            let error = task_runtime::launch(&mut env, &root, &spec).unwrap_err();
            assert!(!error.message.contains("synthetic-secret"));
            assert!(!root.join("runtime.json").exists());
            assert!(!root.join("runtime.lock").exists());
            assert!(!root.join("server.log").exists());
            assert!(env.child_processes.is_empty());
        }
    }
    destroy_environment(&env).unwrap();
    assert!(!root.exists());
    for name in ["auth.json", "config.toml", "shared.db"] {
        assert_eq!(
            fs::read_to_string(upstream.join(name)).unwrap(),
            "synthetic-user-asset"
        );
    }
}

#[test]
fn runtime_defaults_are_private_and_process_writes_cannot_escape() {
    let _lock = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let temp = temp();
    let mut env = environment(temp.path());
    assert!(task_runtime::prepare(&env, "../escape").is_err());
    let root = task_runtime::prepare(&env, "a1").unwrap();
    let outside = temp.path().join("outside");
    fs::write(&outside, "preserve").unwrap();
    let mut cmd = task_runtime::command(&env, &root, Path::new("/bin/sh")).unwrap();
    let script="printf private > \"$HOME/owned\"; printf cache > \"$XDG_CACHE_HOME/owned\"; printf session > \"$CODEX_HOME/owned\"; printf changed > \"$1\"; sleep 1";
    let mut child = cmd
        .args(["-c", script, "test", outside.to_str().unwrap()])
        .spawn()
        .unwrap();
    env.register_process(child.id() as i32).unwrap();
    child.wait().unwrap();
    assert_eq!(fs::read_to_string(&outside).unwrap(), "preserve");
    for path in ["home/owned", "cache/owned", "c/owned"] {
        assert!(root.join(path).exists());
    }
    assert!(task_runtime::command(&env, temp.path(), Path::new("/bin/sh")).is_err());
    destroy_environment(&env).unwrap();
    assert!(!root.exists());
    assert!(!is_registered("runtime-test").unwrap());
}

#[test]
fn input_packet_pins_current_requirements_and_excludes_developer_transcript() {
    let _lock = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let temp = temp();
    let env = environment(temp.path());
    let (q, _) = setup(&env, "acceptance");
    let state = queue::load_queue(&q).unwrap();
    let wait = &state["waits"]["review"];
    let packet = task_runtime::native::packet(&state, wait, "Read file and report", &[]).unwrap();
    assert_eq!(
        packet["requirement"]["basis"]["original"][0],
        "file must contain version A"
    );
    assert!(packet.get("transcript").is_none());
    assert!(task_runtime::native::packet(&state, wait, "", &[]).is_err());
    let mut changed = state.clone();
    changed["tasks"]["project"]["requirement_revision"] = json!(2);
    assert!(task_runtime::native::packet(&changed, wait, "review", &[]).is_err());
    destroy_environment(&env).unwrap();
}

pub mod common;
#[path = "common/receiver_bootstrap.rs"]
mod receiver_bootstrap;
#[path = "common/rpc_server.rs"]
mod rpc_fixture;

#[test]
#[ignore = "subprocess-only protocol fixture, not a real agent"]
fn native_fixture_worker() {
    use std::os::unix::net::UnixListener;
    let root = std::path::PathBuf::from(std::env::var("HERDR_RUNTIME_FIXTURE").unwrap());
    let config: J = serde_json::from_slice(&fs::read(root.join("fixture.json")).unwrap()).unwrap();
    let listener = UnixListener::bind(root.join("server.sock")).unwrap();
    fs::set_permissions(root.join("server.sock"), fs::Permissions::from_mode(0o600)).unwrap();
    fixture_write(&root.join("fixture-ready.json"), &json!({"ready":true})).unwrap();
    let thread = json!({"id":"11111111-1111-1111-1111-111111111111","cwd":config["cwd"],"modelProvider":"openai","model":"fixture","turns":[]});
    let mut saved = thread;
    for stream in listener.incoming() {
        let mut peer = rpc_fixture::Peer {
            stream: stream.unwrap(),
        };
        peer.initialize_with_result(
            config
                .get("initialization")
                .cloned()
                .unwrap_or(json!({"serverInfo":{"name":"fixture","version":"1"}})),
        );
        loop {
            let mut first = [0u8];
            use std::os::fd::AsRawFd;
            // SAFETY: first is a live one-byte output buffer and the stream owns fd.
            if unsafe {
                libc::recv(
                    peer.stream.as_raw_fd(),
                    first.as_mut_ptr().cast(),
                    1,
                    libc::MSG_PEEK,
                )
            } <= 0
            {
                break;
            }
            let (opcode, payload) = peer.frame();
            if opcode == 8 {
                break;
            }
            let request: J = serde_json::from_slice(&payload).unwrap();
            if let Ok(control) = fixture_read(&root.join("receiver-fixture.json")) {
                let mut calls =
                    fixture_read(&root.join("receiver-calls.json")).unwrap_or(json!([]));
                calls.as_array_mut().unwrap().push(request.clone());
                fixture_write(&root.join("receiver-calls.json"), &calls).unwrap();
                assert_eq!(
                    request["method"], "thread/start",
                    "bootstrap must not send turns"
                );
                let journal = fixture_read(&root.join("receiver.json")).unwrap();
                assert_eq!(journal["phase"], "creating", "intent must precede request");
                for lock_root in [&root, Path::new(journal["guard_dir"].as_str().unwrap())] {
                    let file = fs::File::open(lock_root.join("guard.lock")).unwrap();
                    // SAFETY: file owns a live descriptor. Probe only; never wait.
                    let result =
                        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
                    assert_eq!(result, -1, "bootstrap must retain both locks");
                    assert_eq!(
                        std::io::Error::last_os_error().raw_os_error(),
                        Some(libc::EWOULDBLOCK)
                    );
                }
                assert!(!root.join("creation.json").exists());
                if control["mode"] == "disconnect" {
                    break;
                }
                if control["mode"] == "rpc-error" {
                    peer.json(&json!({"id":request["id"],"error":{"code":-32602,"message":"synthetic rejection"}}));
                    continue;
                }
                if control["mode"] == "state-write-error" {
                    fs::create_dir(root.join("receiver/state.json")).unwrap();
                }
                if let Some(result) = control.get("response") {
                    peer.json(&json!({"id":request["id"],"result":result}));
                    continue;
                }
            } else if request["method"] == "thread/start" {
                assert_eq!(
                    request["params"]["baseInstructions"],
                    "Perform only the supplied task packet. Report evidence and blockers.",
                    "worker instructions must remain unchanged"
                );
            }
            if request["method"] == "thread/turns/list" {
                peer.json(&json!({"id":request["id"],"error":{"code":-32601,"message":"fixture uses native events"}}));
                continue;
            }
            let result = match request["method"].as_str().unwrap() {
                "thread/start" => {
                    json!({"thread":saved,"model":"fixture","modelProvider":"openai"})
                }
                "thread/read" | "thread/resume" => {
                    json!({"thread":fixture_read(&root.join("thread-override.json")).unwrap_or_else(|_| saved.clone())})
                }
                "thread/turns/list" => json!({"data":saved["turns"],"nextCursor":null}),
                "turn/start" => {
                    saved["turns"] = json!([{"id":"turn-fixture","status":"completed","items":[{"type":"agentMessage","text":"fixture report"}]}]);
                    json!({"turn":{"id":"turn-fixture","status":"inProgress"}})
                }
                other => panic!("unexpected method {other}"),
            };
            peer.json(&json!({"id":request["id"],"result":result}));
            if request["method"] == "turn/start" {
                peer.json(&json!({"method":"turn/completed","params":{"threadId":saved["id"],"turn":saved["turns"][0]}}));
            }
        }
    }
}

fn fixture_binary(root: &Path, cwd: &Path, port: u16) -> std::path::PathBuf {
    fs::write(
        root.join("fixture.json"),
        serde_json::to_vec(&json!({"cwd":cwd,"port":port})).unwrap(),
    )
    .unwrap();
    let binary = root.join("fixture.sh");
    // Test paths are controlled, nevertheless use shell single-quote escaping.
    let quote = |p: &Path| format!("'{}'", p.display().to_string().replace('\'', "'\\''"));
    fs::write(&binary,format!("#!/bin/sh\nexport HERDR_RUNTIME_FIXTURE={}\nexec {} --exact native_fixture_worker --ignored --nocapture\n",quote(root),quote(&std::env::current_exe().unwrap()))).unwrap();
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
    binary
}

fn fixture_log(root: &Path) -> String {
    use std::io::Read;
    let mut bytes = Vec::new();
    if let Ok(file) = fs::File::open(root.join("server.log")) {
        let _ = file.take(4096).read_to_end(&mut bytes);
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

fn fixture_ready(root: &Path, child: &mut std::process::Child) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let status = child.try_wait().unwrap();
        assert!(
            status.is_none(),
            "fixture exited {status:?}: {}",
            fixture_log(root)
        );
        if let Ok(ready) = herdr_flow::util::read_json(&root.join("fixture-ready.json")) {
            assert_eq!(ready["ready"], true);
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "fixture readiness timed out: {}",
            fixture_log(root)
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[test]
fn fixture_transport_rejects_an_unexpected_response_shape() {
    let server = rpc_fixture::Server::start(|mut peer| {
        peer.initialize();
        let request = peer.request();
        peer.json(
            &json!({"id":request["id"],"error":{"code":-32602,"message":"invalid native binding"}}),
        );
    });
    let mut client = herdr_flow::codex::rpc::Client::new(server.path.to_str().unwrap());
    assert!(client
        .call("thread/read", &json!({"threadId":"missing"}))
        .is_err());
    server.finish();
}

// These files belong only to the synthetic peer. Its user namespace maps host
// root-owned ancestors to nobody; production trusted-path checks stay unchanged.
fn fixture_read(path: &Path) -> herdr_flow::Result<J> {
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}
fn fixture_write(path: &Path, value: &J) -> herdr_flow::Result<()> {
    fs::write(path, serde_json::to_vec(value)?)?;
    Ok(())
}

#[test]
fn codex_native_creation_recovery_and_version_are_bound() {
    let _lock = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    for (adapter, initialization, expected_version) in [
        (
            "codex",
            json!({"serverInfo":{"name":"fixture","version":"0.163.0"}}),
            json!("0.163.0"),
        ),
        ("codex", json!({"userAgent":"codex/0.999.0"}), J::Null),
        ("codex", json!({"serverInfo":{"version":null}}), J::Null),
        ("codex", json!({"serverInfo":{"version":123}}), J::Null),
        ("codex", json!({"serverInfo":{"version":"  "}}), J::Null),
    ] {
        let temp = temp();
        let mut env = environment(temp.path());
        let (q, cwd) = setup(&env, "acceptance");
        let root = task_runtime::prepare(&env, "review-a1").unwrap();
        // Let the worker retain the listener selected by the kernel. Never
        // release a reserved port and later ask another process to rebind it.
        let binary = fixture_binary(&root, &cwd, 0);
        let config = json!({"cwd":cwd,"port":0,"initialization":initialization});
        herdr_flow::util::atomic_write_json(&root.join("fixture.json"), &config).unwrap();
        let spec = json!({"adapter":"codex","configuration":"isolated-fixture",
            "binary":binary,"cwd":cwd,"driver_binary":env!("CARGO_BIN_EXE_herdr-flow"),
            "model":{"providerID":"openai","id":"fixture"}});
        let mut child = task_runtime::launch(&mut env, &root, &spec)
            .unwrap_or_else(|error| panic!("{error}; fixture log: {}", fixture_log(&root)));
        fixture_ready(&root, &mut child);
        let request =
            json!({"queue_dir":q,"wait":"review","instruction":"independent verification"});
        let result = (|| -> herdr_flow::Result<()> {
            let mut blocked = queue::load_queue(&q)?;
            blocked["tasks"]["other"] = json!({"writer":{"workspace":cwd}});
            queue::save(&q, &mut blocked)?;
            assert!(task_runtime::native::create(&env, &root, &request).is_err());
            assert!(!root.join("creation.json").exists());
            blocked["tasks"].as_object_mut().unwrap().remove("other");
            queue::save(&q, &mut blocked)?;
            fs::write(cwd.join(".herdr-verdict.json"), "inherited report")?;
            assert!(task_runtime::native::create(&env, &root, &request).is_err());
            assert!(!root.join("creation.json").exists());
            fs::remove_file(cwd.join(".herdr-verdict.json"))?;
            let state = task_runtime::native::create(&env, &root, &request)?;
            assert_eq!(
                state["server_version"], expected_version,
                "{adapter}: {initialization}"
            );
            assert_eq!(
                herdr_flow::util::read_json(&root.join("state.json"))?["server_version"],
                expected_version
            );
            assert_eq!(
                state["session"],
                if adapter == "codex" {
                    "11111111-1111-1111-1111-111111111111"
                } else {
                    "sesfixture"
                }
            );
            assert!(task_runtime::launch(&mut env, &root, &spec).is_err());
            assert!(task_runtime::native::create(&env, &root, &request).is_err());
            {
                let mut stale = herdr_flow::util::read_json(&root.join("state.json"))?;
                stale["server_version"] = json!("1.19.3");
                herdr_flow::util::atomic_write_json(&root.join("state.json"), &stale)?;
                let saved_state = fs::read(root.join("state.json"))?;
                let saved_journal = fs::read(root.join("creation.json"))?;
                let original_queue = queue::load_queue(&q)?;
                let mut mismatched = original_queue.clone();
                mismatched["id"] = json!("other-queue");
                queue::save(&q, &mut mismatched)?;
                assert!(task_runtime::native::recover(&env, &root).is_err());
                assert_eq!(fs::read(root.join("state.json"))?, saved_state);
                assert_eq!(fs::read(root.join("creation.json"))?, saved_journal);
                let mut original_queue = original_queue;
                queue::save(&q, &mut original_queue)?;
            }
            if adapter == "codex" {
                let saved_state = fs::read(root.join("state.json"))?;
                let saved_journal = fs::read(root.join("creation.json"))?;
                let original_queue = queue::load_queue(&q)?;
                let foreign = json!({"id":"22222222-2222-2222-2222-222222222222", "cwd":cwd,
                    "modelProvider":"openai","turns":[]});
                herdr_flow::util::atomic_write_json(&root.join("thread-override.json"), &foreign)?;
                let error = task_runtime::native::recover(&env, &root).unwrap_err();
                assert!(
                    error
                        .to_string()
                        .contains("thread identity/directory mismatch"),
                    "{error}"
                );
                assert_eq!(fs::read(root.join("state.json"))?, saved_state);
                assert_eq!(fs::read(root.join("creation.json"))?, saved_journal);
                assert_eq!(queue::load_queue(&q)?, original_queue);
                fs::remove_file(root.join("thread-override.json"))?;
            }
            let recovered = task_runtime::native::recover(&env, &root)?;
            assert_eq!(recovered["server_version"], expected_version);
            assert_eq!(
                herdr_flow::util::read_json(&root.join("state.json"))?["server_version"],
                expected_version
            );
            assert!(task_runtime::native::dispatch(&env, &root).is_err()); // no coordinator readiness
            Ok(())
        })();
        destroy_environment(&env).unwrap();
        let _ = child.wait();
        result.unwrap();
    }
}
