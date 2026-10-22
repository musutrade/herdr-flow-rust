//! Real native role execution; coordinator/notification receiver is a test harness.
use super::*;
use herdr_flow::{herdr::receiver_factory, util};
pub(super) fn change(q: &Path, mut spec: J) -> J {
    let _lock = queue::locked(q, true).unwrap();
    let mut s = queue::load_queue(q).unwrap();
    spec["task"] = json!("project");
    spec["state_version"] = s["tasks"]["project"]["state_version"].clone();
    let result = progress::lineage::action(&mut s, &spec, util::now()).unwrap();
    queue::save(q, &mut s).unwrap();
    result
}
pub(super) fn activity(name: &str, role: &str, input: &J) -> J {
    json!({"activity":name,"role":role,"input":input,"session_epoch":1,
        "result_wait":{"id":name,"task":name,"kind":"signal","epoch":1,"assignment":name,"owner":role,"reason":"native role result","condition":"terminal report","timeout_seconds":600}})
}
fn port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}
fn ready(port: u16) {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .proxy(None)
        .timeout_global(Some(std::time::Duration::from_secs(2)))
        .build()
        .into();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        if let Ok(mut response) = agent
            .get(format!("http://127.0.0.1:{port}/global/health"))
            .call()
        {
            if let Ok(value) = response.body_mut().read_json::<J>() {
                if value["healthy"] == true {
                    return;
                }
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "native readiness deadline"
        );
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}
// Receiver preflight may be briefly unavailable while its real watcher starts.
// This outer helper cannot resume an existing runtime dispatch journal. Lock
// contention inside that journal is handled by native::dispatch itself using
// the adapter's submission journal as its no-resend boundary.
fn retry_unstarted(
    root: &Path,
    journal: &str,
    mut action: impl FnMut() -> herdr_flow::Result<J>,
) -> herdr_flow::Result<J> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        match action() {
            Err(error)
                if !root.join(journal).exists()
                    && (error.kind == herdr_flow::error::ErrorKind::StateBusy
                        || error.message
                            == "coordinator receiver must be ready before dispatch")
                    && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            result => return result,
        }
    }
}

#[test]
fn native_driver_retries_busy_only_before_a_creation_or_dispatch_journal() {
    let temp = temp();
    let mut calls = 0;
    let result = retry_unstarted(temp.path(), "dispatch.json", || {
        calls += 1;
        if calls == 1 {
            Err(herdr_flow::FlowError::new(
                herdr_flow::error::ErrorKind::StateBusy,
                "fixture busy",
            ))
        } else {
            Ok(json!({"ok":true}))
        }
    })
    .unwrap();
    assert_eq!(result["ok"], true);
    assert_eq!(calls, 2);
    calls = 0;
    assert!(retry_unstarted(temp.path(), "dispatch.json", || {
        calls += 1;
        fs::write(temp.path().join("dispatch.json"), "uncertain").unwrap();
        Err(herdr_flow::FlowError::new(
            herdr_flow::error::ErrorKind::StateBusy,
            "fixture busy after journal",
        ))
    })
    .is_err());
    assert_eq!(calls, 1);
}

pub(super) fn require_nested_userns() {
    let child = std::process::Command::new("bwrap")
        .args([
            "--unshare-user",
            "--ro-bind",
            "/",
            "/",
            "--",
            "bwrap",
            "--unshare-user",
            "--ro-bind",
            "/",
            "/",
            "--",
            "/bin/true",
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap_or_else(|error| panic!("nested user namespace preflight unavailable: {error}"));
    let mut child = NativeChild::new(child);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if let Some(status) = child.child_mut().try_wait().unwrap() {
            assert!(
                status.success(),
                "nested user namespace preflight failed: this host cannot run Codex's bubblewrap bootstrap inside the task boundary"
            );
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "nested user namespace preflight timeout"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

struct NativeChild {
    child: Option<std::process::Child>,
}

impl NativeChild {
    fn new(child: std::process::Child) -> Self {
        Self { child: Some(child) }
    }

    fn child_mut(&mut self) -> &mut std::process::Child {
        self.child.as_mut().unwrap()
    }

    fn stop(&mut self) {
        self.terminate()
            .unwrap_or_else(|error| panic!("native child cleanup failed: {error}"));
    }

    fn terminate(&mut self) -> std::io::Result<()> {
        let Some(child) = self.child.as_mut() else {
            return Ok(());
        };
        if child.try_wait()?.is_none() {
            child.kill()?;
            wait_bounded_child(child)?;
        }
        self.child.take();
        Ok(())
    }
}

impl Drop for NativeChild {
    fn drop(&mut self) {
        if let Err(error) = self.terminate() {
            eprintln!("native child cleanup failed: {error}");
        }
    }
}

fn wait_bounded_child(child: &mut std::process::Child) -> std::io::Result<()> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if child.try_wait()?.is_some() {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "native child did not exit within 5s after kill",
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

#[test]
fn native_child_guard_reaps_a_dropped_process() {
    let child = std::process::Command::new("sleep")
        .arg("30")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let pid = child.id();
    drop(NativeChild::new(child));
    std::thread::sleep(std::time::Duration::from_millis(100));
    assert!(
        !std::path::Path::new(&format!("/proc/{pid}")).exists(),
        "native child was not reaped"
    );
}

pub(super) fn native(
    env: &mut TaskEnvironment,
    q: &Path,
    cwd: &Path,
    name: &str,
    instruction: &str,
) -> (J, J) {
    native_with_hook(env, q, cwd, name, instruction, || {}, |_, _, _| {})
}

pub(super) struct NativeTurn {
    pub(super) root: std::path::PathBuf,
    pub(super) cwd: std::path::PathBuf,
    pub(super) endpoint: String,
    pub(super) client: ureq::Agent,
    state: J,
    child: NativeChild,
}

impl NativeTurn {
    pub(super) fn stop(&mut self) {
        self.child.stop();
    }
}

pub(super) fn native_prepare(
    env: &mut TaskEnvironment,
    q: &Path,
    cwd: &Path,
    name: &str,
    instruction: &str,
) -> NativeTurn {
    let root = task_runtime::prepare(env, name).unwrap();
    common::create_dir_all(root.join("config/opencode")).unwrap();
    fs::copy(
        std::env::var("HERDR_LIVE_OPENCODE_CONFIG").unwrap(),
        root.join("config/opencode/opencode.json"),
    )
    .unwrap();
    // This bounded test measures explicit roles, without internal delegation.
    // Only the disposable config copy is changed; native approvals remain native.
    let config_path = root.join("config/opencode/opencode.json");
    let mut config = util::read_json(&config_path).unwrap();
    config["tools"]["task"] = json!(false);
    fs::write(&config_path, serde_json::to_vec(&config).unwrap()).unwrap();
    let model: J =
        serde_json::from_str(&std::env::var("HERDR_LIVE_OPENCODE_MODEL").unwrap()).unwrap();
    let spec = json!({"adapter":"opencode","binary":std::env::var("HERDR_LIVE_OPENCODE_BINARY").unwrap(),"cwd":cwd,"model":model,"port":port()});
    let child = NativeChild::new(task_runtime::launch(env, &root, &spec).unwrap());
    ready(spec["port"].as_u64().unwrap() as u16);
    let instruction=format!("{instruction} Use direct tools only. Do not delegate, use task tools, or create child sessions. Stay inside the supplied cwd.");
    let state = retry_unstarted(&root, "creation.json", || {
        task_runtime::native::create(
            env,
            &root,
            &json!({"queue_dir":q,"wait":name,"instruction":instruction}),
        )
    })
    .unwrap();
    let endpoint = format!("http://127.0.0.1:{}", spec["port"].as_u64().unwrap());
    let client: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(std::time::Duration::from_secs(2)))
        .build()
        .into();
    NativeTurn {
        root,
        cwd: cwd.to_path_buf(),
        endpoint,
        client,
        state,
        child,
    }
}

pub(super) fn native_dispatch(env: &TaskEnvironment, turn: &NativeTurn) {
    retry_unstarted(&turn.root, "dispatch.json", || {
        task_runtime::native::dispatch(env, &turn.root)
    })
    .unwrap();
}

pub(super) fn native_start(
    env: &mut TaskEnvironment,
    q: &Path,
    cwd: &Path,
    name: &str,
    instruction: &str,
) -> NativeTurn {
    let mut turn = native_prepare(env, q, cwd, name, instruction);
    native_dispatch(env, &turn);
    turn
}

pub(super) fn native_finish(env: &TaskEnvironment, turn: &mut NativeTurn) -> J {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(180);
    let result = loop {
        let collected = task_runtime::native::collect(env, &turn.root);
        if collected
            .as_ref()
            .is_err_and(|error| error.kind == herdr_flow::error::ErrorKind::StateBusy)
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(50));
            continue;
        }
        let r = collected.unwrap_or_else(|error| {
            panic!(
                "native collection: {error}; report={}",
                fs::read_to_string(turn.cwd.join(".herdr-verdict.json")).unwrap_or_default()
            )
        });
        if r["phase"] == "signalled" {
            break r;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "native role {} unresolved: {}",
            turn.state["session"],
            r["phase"]
        );
        std::thread::sleep(std::time::Duration::from_millis(500));
    };
    assert_eq!(result["outcome"], "complete");
    result
}

pub(super) fn native_with_hook<F: FnOnce(), G: FnOnce(&ureq::Agent, &str, &J)>(
    env: &mut TaskEnvironment,
    q: &Path,
    cwd: &Path,
    name: &str,
    instruction: &str,
    after_dispatch: F,
    after_collect: G,
) -> (J, J) {
    let mut turn = native_start(env, q, cwd, name, instruction);
    after_dispatch();
    let result = native_finish(env, &mut turn);
    after_collect(&turn.client, &turn.endpoint, &result);
    turn.stop();
    println!("real native {name}: exact session collected and server stopped");
    (turn.state, result)
}

pub(super) fn native_codex(
    env: &mut TaskEnvironment,
    q: &Path,
    cwd: &Path,
    root: &Path,
    name: &str,
    instruction: &str,
) -> (J, J) {
    let config_source = std::env::var_os("HERDR_LIVE_CODEX_CONFIG")
        .map(std::path::PathBuf::from)
        .expect("HERDR_LIVE_CODEX_CONFIG required for Codex developer");
    let config_before = fs::read(&config_source).unwrap();
    fs::copy(&config_source, root.join("c/config.toml")).unwrap();
    let auth_source = std::env::var_os("HERDR_LIVE_CODEX_AUTH").map(std::path::PathBuf::from);
    let auth_before = auth_source.as_ref().map(|path| fs::read(path).unwrap());
    if let Some(path) = &auth_source {
        fs::copy(path, root.join("c/auth.json")).unwrap();
    }
    let binary = std::env::var("HERDR_LIVE_CODEX_BINARY").unwrap();
    let model: J = serde_json::from_str(&std::env::var("HERDR_LIVE_CODEX_MODEL").unwrap()).unwrap();
    let spec = json!({
        "adapter":"codex",
        "approval_policy":"on-request",
        "driver_binary":env!("CARGO_BIN_EXE_herdr-flow"),
        "binary":binary,
        "cwd":cwd,
        "model":model
    });
    let mut child = NativeChild::new(task_runtime::launch(env, root, &spec).unwrap());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while !root.join("server.sock").exists() {
        assert!(
            child.child_mut().try_wait().unwrap().is_none(),
            "Codex app-server exited before socket readiness"
        );
        assert!(
            std::time::Instant::now() < deadline,
            "Codex app-server socket readiness deadline"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let instruction = format!("{instruction} Use direct tools only. Do not delegate, use task tools, or create child sessions. Stay inside the supplied cwd.");
    let state = retry_unstarted(root, "creation.json", || {
        task_runtime::native::create(
            env,
            root,
            &json!({"queue_dir":q,"wait":name,"instruction":instruction}),
        )
    })
    .unwrap();
    retry_unstarted(root, "turn.json", || {
        task_runtime::native::dispatch(env, root)
    })
    .unwrap();
    assert_eq!(state["adapter"], "codex");
    assert!(state["session"]
        .as_str()
        .is_some_and(|session| !session.is_empty()));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(900);
    let result = loop {
        let collected = task_runtime::native::collect(env, root);
        if collected
            .as_ref()
            .is_err_and(|error| error.kind == herdr_flow::error::ErrorKind::StateBusy)
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(50));
            continue;
        }
        let r = collected.unwrap_or_else(|error| panic!("codex native collection: {error}"));
        if r["phase"] == "signalled" {
            break r;
        }
        assert_ne!(
            r["phase"], "blocked",
            "Codex native approval/user-input blocked the role; no automatic approval"
        );
        assert!(
            std::time::Instant::now() < deadline,
            "Codex native role {name} unresolved: {}",
            r["phase"]
        );
        std::thread::sleep(std::time::Duration::from_millis(500));
    };
    assert_eq!(result["outcome"], "complete");
    assert_eq!(result["session"], state["session"]);
    assert!(result["turn"].as_str().is_some_and(|turn| !turn.is_empty()));
    assert_eq!(result["report"]["id"], result["turn"]);
    child.stop();
    assert!(
        fs::read(&config_source).unwrap() == config_before,
        "selected Codex config changed"
    );
    if let (Some(path), Some(before)) = (&auth_source, auth_before) {
        assert!(
            fs::read(path).unwrap() == before,
            "selected Codex auth changed"
        );
    }
    println!("real Codex native {name}: exact turn collected and server stopped");
    (state, result)
}

fn apply(q: &Path, name: &str, result: &J, next: &str) -> J {
    let current = queue::load_queue(q).unwrap();
    let guard = util::read_json(
        &std::path::PathBuf::from(current["guard_dir"].as_str().unwrap()).join("state.json"),
    )
    .unwrap();
    herdr_flow::opencode::session::SessionAdapter::new(&guard)
        .unwrap()
        .reconcile()
        .unwrap();
    let state = queue::tick(q, &receiver_factory, None).unwrap();
    let delivery = state["active"].as_str().expect("actual queued delivery");
    let mut s = queue::acknowledge(q, delivery, "sescoordinator").unwrap();
    let event = s["tasks"][name]["pending"][0].clone();
    let mut spec =
        json!({"task":name,"state_version":s["tasks"][name]["state_version"],"event":event});
    if next == "complete" {
        spec["output"] = result["output"].clone();
        spec["acceptance"] = json!({"goal":"bounded development assignment completed; independent acceptance still required"});
        spec["release_evidence"] = json!("native server stopped after terminal turn");
        spec["next_action"] = json!({"kind":"complete"});
    } else {
        spec["verdict"] = s["tasks"][name]["verdict"]["id"].clone();
        spec["next_action"] = json!({"kind":next,"owner":"coordinator harness","reason":"independent verdict","condition":"existing authorization and recorded gaps"});
    }
    progress::apply(&mut s, &spec, util::now()).unwrap();
    queue::save(q, &mut s).unwrap();
    s
}
pub(super) fn clone_snapshot(
    env: &TaskEnvironment,
    artifact: &J,
    name: &str,
) -> std::path::PathBuf {
    let cwd = env.workspace_root.join(name);
    git(
        &env.workspace_root,
        &[
            "clone",
            "-q",
            artifact["location"].as_str().unwrap(),
            cwd.to_str().unwrap(),
        ],
    );
    git(
        &cwd,
        &["checkout", "--detach", artifact["id"].as_str().unwrap()],
    );
    git(&cwd, &["config", "user.name", "Native fixture"]);
    git(&cwd, &["config", "user.email", "native@example.invalid"]);
    cwd
}
#[test]
#[ignore = "real OpenCode 1.19.3 development, independent failure, repair and independent pass; destroys environment"]
fn installed_real_roles_fail_repair_and_pass() {
    let _lock = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let temp = temp();
    let mut env = environment(temp.path());
    let (q, cwd) = setup(&env, "development");
    git(&cwd, &["config", "user.name", "Native fixture"]);
    git(&cwd, &["config", "user.email", "native@example.invalid"]);
    let mut s = queue::load_queue(&q).unwrap();
    s["tasks"]["project"]["requirements"]["1"]["authorization"]["develop"] =
        json!({"action":"development","source":"explicit disposable native test"});
    queue::save(&q, &mut s).unwrap();
    let coord = task_runtime::prepare(&env, "coordinator-fixture").unwrap();
    let p = port();
    let binary = fixture_binary(&coord, &cwd, p);
    let mut coordinator_child=task_runtime::launch(&mut env,&coord,&json!({"adapter":"opencode","binary":binary,"cwd":cwd,"port":p,"model":{"providerID":"openai","id":"fixture"}})).unwrap();
    for _ in 0..100 {
        if coord.join("server.sock").exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let (g, w) = receiver(&env, &q, &cwd, p);
    // The coordinator is a protocol fixture. Keep its declared liveness bound
    // longer than the four bounded native turns, while holding both watcher locks.
    for root in [env.temp_root.join("coordinator"), q.clone()] {
        let mut watch = util::read_json(&root.join("watch.json")).unwrap();
        watch["max_age"] = json!(900);
        util::atomic_write_json(&root.join("watch.json"), &watch).unwrap();
    }
    let (dev,a)=native(&mut env,&q,&cwd,"review","This is the first stage of an authorized negative integration fixture. Change file to exactly version INCOMPLETE and commit locally. This deliberately incomplete intermediate artifact will be independently reviewed against the unchanged final requirement. Do not create an acceptance report.");
    apply(&q, "review", &a, "complete");
    let a = a["output"].clone();
    change(
        &q,
        json!({"kind":"candidate","candidate":a["id"],"reason":"independent first review"}),
    );
    let mut request = activity("accept-first", "acceptance", &a["id"]);
    request["kind"] = json!("activity");
    change(&q, request);
    let review_cwd = clone_snapshot(&env, &a, "review-first");
    let (reviewer,report)=native(&mut env,&q,&review_cwd,"accept-first","Independently inspect the exact artifact against the supplied original requirement. Do not edit tracked files or commit. Write .herdr-verdict.json following acceptance_protocol, with actual checks, grounded gaps and evidence. Derive the conclusion yourself.");
    assert_ne!(dev["session"], reviewer["session"]);
    let s = apply(&q, "accept-first", &report, "repair");
    assert_eq!(
        s["tasks"]["accept-first"]["verdict"]["envelope"]["report"]["conclusion"],
        "failed"
    );
    assert!(s["tasks"]["project"]["approved"].is_null());
    change(
        &q,
        json!({"kind":"continue","intent":"accept-first","authorization":"develop","action":"development","resolution":"perform recorded repair under existing authorization","activity":activity("repair","development",&a["id"])}),
    );
    let repair_cwd = clone_snapshot(&env, &a, "repair-work");
    let (_,b)=native(&mut env,&q,&repair_cwd,"repair","Fix the recorded gaps to satisfy the original requirement: file must contain exactly version B. Commit the repair locally. Do not write an acceptance report.");
    apply(&q, "repair", &b, "complete");
    let b = b["output"].clone();
    assert_ne!(a["id"], b["id"]);
    change(
        &q,
        json!({"kind":"candidate","candidate":b["id"],"reason":"independent re-verification"}),
    );
    let mut request = activity("accept-final", "acceptance", &b["id"]);
    request["kind"] = json!("activity");
    change(&q, request);
    let final_cwd = clone_snapshot(&env, &b, "review-final");
    let (reviewer2,report)=native(&mut env,&q,&final_cwd,"accept-final","Independently inspect the fixed candidate against the original requirement. Do not edit tracked files or commit. Write .herdr-verdict.json following acceptance_protocol; include evidence-backed resolutions of every open_gaps entry using its activity and gap IDs. Derive the conclusion from your own checks.");
    assert_ne!(reviewer["session"], reviewer2["session"]);
    let s = apply(&q, "accept-final", &report, "delivery");
    assert_eq!(s["tasks"]["project"]["approved"]["candidate"], b["id"]);
    assert_eq!(s["tasks"]["project"]["open_gaps"], json!({}));
    drop((g, w));
    destroy_environment(&env).unwrap();
    coordinator_child.wait().unwrap();
    assert!(!env.workspace_root.exists());
    assert!(!env.temp_root.exists());
    assert!(!is_registered("runtime-test").unwrap());
    println!("real developer -> independent failed verdict -> repair -> fresh independent passed verdict; actual queue delivery/ACK/application and whole environment destruction passed; coordinator is a protocol fixture, no customer delivery claimed");
}
