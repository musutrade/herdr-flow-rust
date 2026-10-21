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
    // delivery (or destroy an owned environment) if the handoff fails.
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
        if !is_registered(&self.task_id).unwrap_or(false) {
            return;
        }
        let mut env = match restore_environment(&self.task_id) {
            Ok(env) => env,
            Err(error) => {
                eprintln!("handoff cleanup could not restore environment: {error}");
                return;
            }
        };
        let phase = env.delivery["phase"].as_str().unwrap_or_default();
        if phase == "confirmed" || phase == "cleaning" {
            if let Err(error) = delivery::finish(&mut env) {
                eprintln!("handoff cleanup finish failed: {error}");
            }
        }
        if is_registered(&self.task_id).unwrap_or(false) {
            if let Err(error) = destroy_environment(&env) {
                eprintln!("handoff cleanup destroy failed: {error}");
            }
        }
        if cleanup_state_path(&self.task_id)
            .map(|path| path.exists())
            .unwrap_or(false)
        {
            if let Err(error) = resume_delivery_cleanup(&self.task_id) {
                eprintln!("handoff cleanup plan resume failed: {error}");
            }
        }
    }
    fn disarm(mut self) {
        self.armed = false;
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
fn environment_project(root: &Path, target: &Path) -> OwnedEnvironment {
    environment_with(
        root,
        "project",
        vec![DeliveryTarget::new(target.to_path_buf(), "git".into())],
    )
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
    let source = env.workspace_root.join("source");
    common::create_dir_all(&source).unwrap();
    git(&source, &["init", "-q"]);
    git(&source, &["config", "user.name", "Test"]);
    git(&source, &["config", "user.email", "test@example.invalid"]);
    fs::write(source.join("file"), "version A").unwrap();
    git(&source, &["add", "file"]);
    git(&source, &["commit", "-qm", "A"]);
    let sha = git(&source, &["rev-parse", "HEAD"]);
    let cwd = env.workspace_root.join("acceptance");
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
    let mut state = queue::load_queue(q).unwrap();
    progress::lineage::action(&mut state,&json!({"kind":"plan","task":"project","goal":"verify file",
        "basis":{"original":[if role=="development" {"change file to version B and commit locally"} else {"file must contain version A"}],"changes":[],"specifications":[],"endpoint":"user delivery","confirmation":"user"},
        "authorization":[{"id":"verify","action":"review","source":"user"}]}),now).unwrap();
    if let Some(target) = env.delivery_targets.first() {
        state["tasks"]["project"]["requirements"]["1"]["authorization"]["deliver"] =
            json!({"action":"delivery","source":"explicit disposable native delivery test"});
        state["tasks"]["project"]["requirements"]["1"]["basis"]["endpoint"] = json!(target.path);
    }
    if role == "development" {
        state["tasks"]["project"]["requirements"]["1"]["authorization"]["develop"] =
            json!({"action":"development","source":"explicit disposable native development test"});
    }
    for spec in [
        json!({"kind":"artifact","artifact":{"id":sha,"adapter":"git","location":source}}),
        json!({"kind":"candidate","candidate":sha,"reason":"review"}),
        json!({"kind":"activity","activity":"review","role":role,"input":sha,"session_epoch":1,
            "result_wait":{"id":"review","kind":"signal","task":"review","epoch":1,"assignment":"review-a1","owner":"reviewer","reason":"review","condition":"report","timeout_seconds":600}}),
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

#[test]
#[ignore = "requires installed Codex and OpenCode 1.19.3; creates and deletes real dedicated sessions"]
fn installed_fresh_sessions_are_bound_and_entirely_destroyed() {
    let _lock = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    for adapter in ["opencode", "codex"] {
        if std::env::var("HERDR_LIVE_ADAPTER").is_ok_and(|v| v != adapter) {
            continue;
        }
        let temp = temp();
        let mut env = environment(temp.path());
        let role = std::env::var("HERDR_LIVE_ROLE").unwrap_or("acceptance".into());
        let (q, cwd) = setup(&env, &role);
        git(&cwd, &["config", "user.name", "Native fixture"]);
        git(&cwd, &["config", "user.email", "native@example.invalid"]);

        let root = task_runtime::prepare(&env, "review-a1").unwrap();
        let key = format!("HERDR_LIVE_{}_CONFIG", adapter.to_uppercase());
        if let Ok(config) = std::env::var(key) {
            let destination = if adapter == "codex" {
                root.join("c/config.toml")
            } else {
                {
                    let dir = root.join("config/opencode");
                    fs::create_dir_all(&dir).unwrap();
                    dir.join("opencode.json")
                }
            };
            fs::copy(config, destination).unwrap();
        }
        if adapter == "codex" {
            if let Ok(auth) = std::env::var("HERDR_LIVE_CODEX_AUTH") {
                fs::copy(auth, root.join("c/auth.json")).unwrap();
            }
        }
        let binary =
            std::env::var(format!("HERDR_LIVE_{}_BINARY", adapter.to_uppercase())).unwrap();
        let model: J = serde_json::from_str(
            &std::env::var(format!("HERDR_LIVE_{}_MODEL", adapter.to_uppercase())).unwrap(),
        )
        .unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let spec = json!({"adapter":adapter,"approval_policy":if role=="development" {"on-request"} else {"untrusted"},"driver_binary":env!("CARGO_BIN_EXE_herdr-flow"),"binary":binary,"cwd":cwd,"model":model,"port":port});
        let mut child = task_runtime::launch(&mut env, &root, &spec).unwrap();
        let result = (|| -> herdr_flow::Result<()> {
            // Startup observation is bounded; creation itself is never retried.
            for _ in 0..100 {
                if adapter == "codex" && fs::symlink_metadata(root.join("server.sock")).is_ok() {
                    break;
                }
                if adapter == "opencode"
                    && std::net::TcpStream::connect(("127.0.0.1", port)).is_ok()
                {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            std::thread::sleep(std::time::Duration::from_secs(2));
            let state = task_runtime::native::create(
                &env,
                &root,
                &json!({"queue_dir":q,"wait":"review","instruction":if role=="development" {"In this temporary repository, replace file with exactly version B and create one local Git commit. You are authorized to edit and commit here. Do not contact external services or change global configuration. Report the resulting commit."} else {"Describe the requirement and propose independent acceptance checks using only the supplied packet. Do not use tools or ask questions. Do not claim acceptance passed."}}),
            )?;
            assert_eq!(state["adapter"], adapter);
            assert!(state["session"].as_str().is_some_and(|s| !s.is_empty()));
            assert!(task_runtime::native::create(
                &env,
                &root,
                &json!({"queue_dir":q,"wait":"review","instruction":"duplicate"})
            )
            .is_err());
            task_runtime::native::recover(&env, &root)?;
            if std::env::var("HERDR_LIVE_INFERENCE").as_deref() == Ok("1") {
                let coord_root = task_runtime::prepare(&env, "coordinator-fixture")?;
                let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
                let coord_port = listener.local_addr()?.port();
                drop(listener);
                let binary = fixture_binary(&coord_root, &cwd, coord_port);
                let _coord_child = task_runtime::launch(
                    &mut env,
                    &coord_root,
                    &json!({"adapter":"opencode","binary":binary,"cwd":cwd,"port":coord_port,"model":{"providerID":"openai","id":"fixture"}}),
                )?;
                for _ in 0..100 {
                    if coord_root.join("server.sock").exists() {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                let (_g, _q) = receiver(&env, &q, &cwd, coord_port);
                task_runtime::native::dispatch(&env, &root)?;
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
                loop {
                    let result = task_runtime::native::collect(&env, &root)?;
                    if result["phase"] == "signalled" {
                        if result["outcome"] != "complete" {
                            return Err(herdr_flow::FlowError::guard(format!(
                                "native response was not complete: {}",
                                result
                            )));
                        }
                        if role == "development" {
                            assert_ne!(result["output"]["id"], result["output"]["input"]);
                            assert_eq!(result["output"]["id"], git(&cwd, &["rev-parse", "HEAD"]));
                            assert_eq!(fs::read_to_string(cwd.join("file"))?.trim(), "version B");
                            println!("live {adapter}: native tool execution created a new Git output commit");
                        }
                        println!("live {adapter}: actual model response collected, coordinator is a protocol fixture");
                        break;
                    }
                    if std::time::Instant::now() >= deadline {
                        return Err(herdr_flow::FlowError::guard(format!(
                            "native response unresolved: {}",
                            result["phase"]
                        )));
                    }
                    std::thread::sleep(std::time::Duration::from_millis(500));
                }
            }
            let queue = queue::load_queue(&q)?;
            assert_eq!(
                queue["waits"]["review"]["producer"]["session"],
                state["session"]
            );
            Ok(())
        })();
        if result.is_err() {
            eprintln!(
                "{adapter}: {:?}; log: {}",
                result,
                fs::read_to_string(root.join("server.log")).unwrap_or_default()
            );
        }
        destroy_environment(&env).unwrap();
        let _ = child.wait();
        assert!(!root.exists());
        assert!(!is_registered("runtime-test").unwrap());
        result.unwrap();
        println!("live {adapter}: fresh session, exact binding, recovery and whole environment destruction passed");
    }
}

pub mod common;
#[path = "common/rpc_server.rs"]
mod rpc_fixture;
#[path = "common/runtime_http.rs"]
mod runtime_http;

#[test]
#[ignore = "subprocess-only protocol fixture, not a real agent"]
fn native_fixture_worker() {
    use std::os::unix::net::UnixListener;
    let root = std::path::PathBuf::from(std::env::var("HERDR_RUNTIME_FIXTURE").unwrap());
    let config: J = serde_json::from_slice(&fs::read(root.join("fixture.json")).unwrap()).unwrap();
    let http = std::net::TcpListener::bind(("127.0.0.1", config["port"].as_u64().unwrap() as u16))
        .unwrap();
    let port = http.local_addr().unwrap().port();
    let cwd = std::path::PathBuf::from(config["cwd"].as_str().unwrap());
    std::thread::spawn(move || runtime_http::serve(http, &cwd));
    let listener = UnixListener::bind(root.join("server.sock")).unwrap();
    fs::set_permissions(root.join("server.sock"), fs::Permissions::from_mode(0o600)).unwrap();
    herdr_flow::util::atomic_write_json(&root.join("fixture-ready.json"), &json!({"port":port}))
        .unwrap();
    let thread = json!({"id":"11111111-1111-1111-1111-111111111111","cwd":config["cwd"],"modelProvider":"openai","model":"fixture","turns":[]});
    let mut saved = thread;
    for stream in listener.incoming() {
        let mut peer = rpc_fixture::Peer {
            stream: stream.unwrap(),
        };
        peer.initialize();
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
            if request["method"] == "thread/turns/list" {
                peer.json(&json!({"id":request["id"],"error":{"code":-32601,"message":"fixture uses native events"}}));
                continue;
            }
            let result = match request["method"].as_str().unwrap() {
                "thread/start" => {
                    json!({"thread":saved,"model":"fixture","modelProvider":"openai"})
                }
                "thread/read" | "thread/resume" => json!({"thread":saved}),
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

fn fixture_ready(root: &Path, child: &mut std::process::Child) -> u16 {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let status = child.try_wait().unwrap();
        assert!(
            status.is_none(),
            "fixture exited {status:?}: {}",
            fixture_log(root)
        );
        if let Ok(ready) = herdr_flow::util::read_json(&root.join("fixture-ready.json")) {
            let port = ready["port"].as_u64().unwrap();
            assert!((1..=65535).contains(&port));
            return port as u16;
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
fn dedicated_native_protocol_creation_recovery_and_unknown_send_are_bound() {
    let _lock = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    for adapter in ["codex", "opencode"] {
        let temp = temp();
        let mut env = environment(temp.path());
        let (q, cwd) = setup(&env, "acceptance");
        let root = task_runtime::prepare(&env, "review-a1").unwrap();
        // Let the worker retain the listener selected by the kernel. Never
        // release a reserved port and later ask another process to rebind it.
        let binary = fixture_binary(&root, &cwd, 0);
        // Synthetic process exposes both protocols. Launch via the OpenCode path
        // so CI does not need user namespaces merely to exercise RPC fixtures.
        // The synthetic binary ignores launch argv; replace the placeholder
        // runtime port with the readiness handshake before any HTTP client.
        let spec = json!({"adapter":"opencode","binary":binary,"cwd":cwd,"port":1,"model":{"providerID":"openai","id":"fixture"}});
        let mut child = task_runtime::launch(&mut env, &root, &spec)
            .unwrap_or_else(|error| panic!("{error}; fixture log: {}", fixture_log(&root)));
        let port = fixture_ready(&root, &mut child);
        let mut runtime = herdr_flow::util::read_json(&root.join("runtime.json")).unwrap();
        runtime["adapter"] = json!(adapter);
        runtime["port"] = json!(port);
        herdr_flow::util::atomic_write_json(&root.join("runtime.json"), &runtime).unwrap();
        if adapter == "codex" {
            task_runtime::start_broker(
                &mut env,
                &root,
                &json!({"driver_binary":env!("CARGO_BIN_EXE_herdr-flow")}),
            )
            .unwrap();
            for _ in 0..100 {
                if root.join("broker.sock").exists() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
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
                state["session"],
                if adapter == "codex" {
                    "11111111-1111-1111-1111-111111111111"
                } else {
                    "sesfixture"
                }
            );
            assert!(task_runtime::launch(&mut env, &root, &spec).is_err());
            assert!(task_runtime::native::create(&env, &root, &request).is_err());
            task_runtime::native::recover(&env, &root)?;
            assert!(task_runtime::native::dispatch(&env, &root).is_err()); // no coordinator readiness
            let (_g, _q) = receiver(&env, &q, &cwd, port);
            let turn = task_runtime::native::dispatch(&env, &root)?;
            assert_eq!(turn["phase"], "submitted");
            assert!(task_runtime::native::dispatch(&env, &root).is_err());
            std::fs::write(
                cwd.join(".herdr-verdict.json"),
                serde_json::to_vec(
                    &json!({"conclusion":"passed","scope":"file","checks":[{"id":"content","criterion":"goal","source":"file must contain version A","status":"passed","evidence":"fixture check"}],"gaps":[],"reuses":[],"resolutions":[]}),
                )?,
            )?;
            let result = task_runtime::native::collect(&env, &root)?;
            assert_eq!(result["phase"], "signalled");
            let collected = queue::load_queue(&q)?;
            assert_eq!(
                collected["tasks"]["review"]["verdict"]["envelope"]["reviewer"]["adapter"],
                adapter
            );
            assert!(collected["tasks"]["project"]["approved"].is_null());
            assert_eq!(
                task_runtime::native::collect(&env, &root)?["outcome"],
                "complete"
            );
            let mut journal = herdr_flow::util::read_json(&root.join("creation.json"))?;
            journal["session"] = json!("different-session");
            herdr_flow::util::atomic_write_json(&root.join("creation.json"), &journal)?;
            assert!(task_runtime::native::recover(&env, &root).is_err());
            Ok(())
        })();
        destroy_environment(&env).unwrap();
        let _ = child.wait();
        result.unwrap();
    }
}

fn receiver(
    env: &TaskEnvironment,
    q: &Path,
    cwd: &Path,
    port: u16,
) -> (herdr_flow::util::FileLock, herdr_flow::util::FileLock) {
    let g = env.temp_root.join("coordinator");
    herdr_flow::util::ensure_private_dir(&g, true).unwrap();
    let state = json!({"adapter":"opencode","version":2,"server_version":"1.19.3","state_dir":g,"endpoint":format!("http://127.0.0.1:{port}"),
        "session":"sescoordinator","cwd":cwd,"model":{"providerID":"openai","id":"fixture"},"auto_rotate":false,"phase":"monitoring"});
    herdr_flow::util::atomic_write_json(&g.join("state.json"), &state).unwrap();
    let watch = |p: &Path| {
        herdr_flow::util::atomic_write_json(&p.join("watch.json"),&json!({"status":"running","pid":std::process::id(),"start_ticks":herdr_flow::util::process_start_ticks(std::process::id()),"heartbeat_at":herdr_flow::util::now(),"max_age":60})).unwrap();
        herdr_flow::util::FileLock::acquire(&p.join("watch.lock"), false)
            .unwrap()
            .unwrap()
    };
    let locks = (watch(&g), watch(q));
    let mut state = queue::load_queue(q).unwrap();
    state["guard_dir"] = json!(g);
    state["receiver"] =
        herdr_flow::guard::scope(&herdr_flow::util::read_json(&g.join("state.json")).unwrap());
    queue::enable(q, &mut state, &herdr_flow::herdr::receiver_factory).unwrap();
    locks
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

#[path = "common/acceptance_live.rs"]
mod acceptance_live;

fn owned_foreground_watcher(
    env: &mut TaskEnvironment,
    kind: &str,
    root: &Path,
) -> std::process::Child {
    let log = fs::File::create(root.join("foreground.log")).unwrap();
    let mut child = env
        .command(env!("CARGO_BIN_EXE_herdr-flow"))
        .args([kind, "watch", "--state-dir"])
        .arg(root)
        .stdout(log.try_clone().unwrap())
        .stderr(log)
        .spawn()
        .unwrap();
    if let Err(error) = env.register_process(child.id() as i32) {
        let _ = child.kill();
        let _ = child.wait();
        panic!("watcher registration: {error}");
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        let status = child.try_wait().unwrap();
        assert!(status.is_none(), "watcher exited: {status:?}");
        let health = herdr_flow::watch_service::health(root);
        if health["healthy"] == true {
            assert_eq!(health["pid"], child.id());
            return child;
        }
        assert!(std::time::Instant::now() < deadline, "{health}");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

#[test]
fn production_foreground_watchers_are_owned_and_reclaimed() {
    let _lock = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let temp = temp();
    let mut env = environment(temp.path());
    let cwd = env.workspace_root.join("receiver");
    fs::create_dir(&cwd).unwrap();
    let root = task_runtime::prepare(&env, "receiver-server").unwrap();
    let binary = fixture_binary(&root, &cwd, 0);
    let mut server = task_runtime::launch(
        &mut env,
        &root,
        &json!({"adapter":"opencode","binary":binary,"cwd":cwd,"port":1,
            "model":{"providerID":"openai","id":"fixture"}}),
    )
    .unwrap();
    let port = fixture_ready(&root, &mut server);
    let guard = env.temp_root.join("guard");
    // Real registration and watcher processes; only the native HTTP peer is synthetic.
    herdr_flow::opencode::session::register(
        &guard,
        &json!({"endpoint":format!("http://127.0.0.1:{port}"),"cwd":cwd,
            "session":"sescoordinator","model":{"providerID":"openai","id":"fixture"}}),
    )
    .unwrap();
    let mut guard_child = owned_foreground_watcher(&mut env, "opencode", &guard);
    let q = env.temp_root.join("queue");
    let mut state = queue::init(&q, Some(&guard), 10).unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        match queue::enable(&q, &mut state, &herdr_flow::herdr::receiver_factory) {
            Ok(()) => break,
            Err(error) if error.kind == herdr_flow::error::ErrorKind::StateBusy => {
                assert!(std::time::Instant::now() < deadline, "{error}");
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(error) => panic!("enable failed: {error}"),
        }
    }
    let mut queue_child = owned_foreground_watcher(&mut env, "queue", &q);
    for child in [&server, &guard_child, &queue_child] {
        assert!(env.owns_process(child.id() as i32));
    }
    destroy_environment(&env).unwrap();
    for child in [&mut server, &mut guard_child, &mut queue_child] {
        assert!(!child.wait().unwrap().success());
    }
    assert!(!env.workspace_root.exists());
    assert!(!env.temp_root.exists());
    assert!(!is_registered(&env.task_id).unwrap());
    assert_eq!(occupied_slots().unwrap(), 0);
}

#[path = "common/receiver_live.rs"]
mod receiver_live;
