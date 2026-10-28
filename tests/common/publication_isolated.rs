use super::*;
use herdr_flow::{
    ci::GitHubApi,
    delivery::publication,
    progress::{acceptance, lineage},
};
use std::{
    ffi::OsString,
    io::Write,
    os::fd::AsRawFd,
    sync::Arc,
    time::{Duration, Instant},
};

const PUBLISHER: &str = "66666666-6666-6666-6666-666666666666";

struct Target<'a>(&'a Fixture);
impl GitHubApi for Target<'_> {
    fn get(&self, suffix: &str) -> herdr_flow::Result<J> {
        let _lock = queue::locked(&self.0.queue, false)?;
        assert_eq!(suffix, "branches/main");
        Ok(json!({"commit":{"sha":self.0.commit}}))
    }
}

fn action(state: &mut J, mut spec: J) {
    spec["task"] = json!("task");
    spec["state_version"] = state["tasks"]["task"]["state_version"].clone();
    lineage::action(state, &spec, 1.0).unwrap();
}

fn approve(state: &mut J, head: &str) {
    action(
        state,
        json!({"kind":"candidate","candidate":head,"reason":"select reviewed commit"}),
    );
    action(
        state,
        json!({"kind":"activity","activity":"review","role":"acceptance","input":head,"session_epoch":1,
        "result_wait":{"id":"review","epoch":1,"assignment":"review","kind":"signal","owner":"acceptance",
        "reason":"review","condition":"report","timeout_seconds":60,"producer":{"adapter":"codex","session":"independent"}}}),
    );
    let report = json!({"conclusion":"passed","scope":"agreed requirement","checks":[{"id":"goal","criterion":"goal",
        "source":"user request","status":"passed","evidence":"reviewed exact checkout commit"}],"gaps":[],"reuses":[],"resolutions":[]});
    let verdict = acceptance::submit(
        state,
        "review",
        &json!({"adapter":"codex","session":"independent"}),
        &report,
    )
    .unwrap();
    let wait = state["waits"]["review"].clone();
    let mut event = json!({"id":"review-result","task":"review","wait":"review","epoch":wait["epoch"],"acknowledged":true,"outcome":"complete"});
    progress::attach(state, &mut event, &wait, 2.0).unwrap();
    state["events"]["review-result"] = event;
    let spec = json!({"task":"review","state_version":state["tasks"]["review"]["state_version"],
        "event":"review-result","verdict":verdict["id"],"next_action":{"kind":"delivery","owner":"host",
        "reason":"independent review passed","condition":"authorized target"}});
    progress::apply(state, &spec, 3.0).unwrap();
}

fn accepted_environment(f: &Fixture) -> task_env::TaskEnvironment {
    git(
        &f.temp.path().join("repo"),
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/owner/repo.git",
        ],
    );
    let mut state = f.state();
    state["tasks"] = json!({});
    lineage::action(&mut state, &json!({"kind":"plan","task":"task","goal":"publish reviewed change",
        "basis":{"original":["user request"],"changes":[],"specifications":[],
        "endpoint":"https://github.com/owner/repo/tree/main","confirmation":"verified remote landing"},
        "authorization":[{"id":"deliver","action":"delivery","source":"user request"},
        {"id":"publish","action":"publication","source":"user request"},
        {"id":"merge","action":"merge","source":"user request"}]}),1.0).unwrap();
    action(
        &mut state,
        json!({"kind":"artifact","artifact":{"adapter":"git","id":f.commit,"location":f.temp.path().join("repo")}}),
    );
    action(
        &mut state,
        json!({"kind":"candidate","candidate":f.commit,"reason":"environment base"}),
    );
    queue::save(&f.queue, &mut state).unwrap();
    f.call(&f.create_spec()).unwrap();
    let env = task_env::restore_environment("task").unwrap();
    let checkout = env.workspace_root.join("checkout");
    git(
        &checkout,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.test",
            "commit",
            "--allow-empty",
            "-qm",
            "reviewed change",
        ],
    );
    let head = git(&checkout, &["rev-parse", "HEAD"]);
    state = f.state();
    // This test has no live receiver. Production binds it under the instance.
    state.as_object_mut().unwrap().remove("guard_dir");
    state.as_object_mut().unwrap().remove("receiver");
    action(
        &mut state,
        json!({"kind":"artifact","artifact":{"adapter":"git","id":head,"location":checkout}}),
    );
    approve(&mut state, &head);
    queue::save(&f.queue, &mut state).unwrap();
    env
}

#[test]
fn isolated_publication_uses_the_accepted_checkout_and_inherits_lifecycle_owner() {
    let f = Fixture::new();
    let mut env = accepted_environment(&f);
    let checkout = env.workspace_root.join("checkout");
    let head = git(&checkout, &["rev-parse", "HEAD"]);
    let mut state = f.state();
    let request = json!({"id":"isolated","state_version":state["tasks"]["task"]["state_version"],
        "authorization":"deliver","publication_authorization":"publish","merge_authorization":"merge",
        "original_base":f.commit,"issue":130,"title":"Reviewed change","body":"Closes #130",
        "policy":{"host":"github.com","repo":"owner/repo","base_ref":"main",
        "endpoint":"https://github.com/owner/repo/tree/main","merge_method":"merge","workflows":[{"file":"CI.yml","event":"pull_request","decision":"required_jobs","jobs":["test"]}]}});
    publication::prepare_remote(&mut env, &f.queue, &request, &Target(&f)).unwrap();
    state = f.state();
    let child = &state["tasks"]["publish-isolated"];
    assert_eq!(child["pr"]["checkout"], json!(checkout));
    assert_eq!(child["pr"]["head"], head);
    assert_eq!(
        herdr_flow::lifecycle::task_owner(&state, "publish-isolated").unwrap(),
        task_env::isolation::owner(&env)
    );
    assert!(lineage::actionable(&state, child));
    assert!(env.delivery_targets.is_empty());
}

struct GhPathGuard(Vec<(&'static str, Option<OsString>)>);

impl GhPathGuard {
    fn install(f: &Fixture) -> Self {
        let bin = f.temp.path().join("fake-gh-bin");
        util::ensure_private_dir(&bin, true).unwrap();
        fs::write(bin.join("gh"), "#!/bin/sh\n[ \"$*\" = 'api --hostname github.com --method GET repos/owner/repo/branches/main' ] || exit 99\nprintf '%s\\n' \"$*\" >> \"$HERDR_TEST_PUBLISH_GH_CALLS\"\nprintf '{\"commit\":{\"sha\":\"%s\"}}\\n' \"$HERDR_TEST_PUBLISH_GH_BASE\"\n").unwrap();
        fs::set_permissions(bin.join("gh"), fs::Permissions::from_mode(0o700)).unwrap();
        let mut paths = vec![bin];
        paths.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        Self::set([
            ("PATH", std::env::join_paths(paths).unwrap()),
            (
                "HERDR_TEST_PUBLISH_GH_CALLS",
                f.temp.path().join("gh-calls").into_os_string(),
            ),
            ("HERDR_TEST_PUBLISH_GH_BASE", f.commit.clone().into()),
        ])
    }

    fn set(values: [(&'static str, OsString); 3]) -> Self {
        let previous = values
            .iter()
            .map(|(name, _)| (*name, std::env::var_os(name)))
            .collect();
        for (name, value) in values {
            std::env::set_var(name, value);
        }
        Self(previous)
    }
}

impl Drop for GhPathGuard {
    fn drop(&mut self) {
        for (name, previous) in &self.0 {
            match previous {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }
}

fn publisher_policy() -> J {
    json!({"host":"github.com","repo":"owner/repo","base_ref":"main",
        "endpoint":"https://github.com/owner/repo/tree/main","merge_method":"merge",
        "workflows":[{"file":"CI.yml","event":"pull_request","decision":"required_jobs","jobs":["test"]}],
        "landing_workflows":[]})
}

fn publisher_source(socket: &Path, cwd: &Path) -> J {
    json!({"adapter":"codex","configuration":"upstream","socket":socket,"cwd":cwd,
        "model":{"providerID":"existing","id":"coordinator-explicit","reasoning_effort":"high"},
        "role_models":{"developer":{"providerID":"existing","id":"developer-explicit"},
        "reviewer":{"providerID":"existing","id":"reviewer-explicit"},
        "publisher":{"providerID":"existing","id":"publisher-explicit","reasoning_effort":"medium"}},
        "publication":publisher_policy(),"driver_binary":env!("CARGO_BIN_EXE_herdr-flow")})
}

fn publisher_server(
    coordinator: &Path,
    cwd: &Path,
    requests: Arc<Mutex<Vec<J>>>,
) -> rpc_server::Server {
    let coordinator = coordinator.to_path_buf();
    let cwd = cwd.to_path_buf();
    // Two real brokers, two recorded-session deletes, one descendant inventory.
    coordinator_continuation::parallel(5, move |peer| {
        serve_publisher(peer, &coordinator, &cwd, &requests)
    })
}

fn serve_publisher(
    mut peer: rpc_server::Peer,
    coordinator: &Path,
    cwd: &Path,
    requests: &Mutex<Vec<J>>,
) {
    peer.stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    peer.initialize();
    let mut turns = json!([]);
    while let Some(request) = publisher_request(&mut peer) {
        requests.lock().unwrap().push(request.clone());
        let result = publisher_response(&request, coordinator, cwd, &mut turns);
        peer.json(&json!({"id":request["id"],"result":result}));
        if request["method"] == "thread/delete" {
            break;
        }
    }
}

fn publisher_request(peer: &mut rpc_server::Peer) -> Option<J> {
    let mut byte = [0u8];
    // SAFETY: the live stream owns its descriptor and byte is writable.
    if unsafe {
        libc::recv(
            peer.stream.as_raw_fd(),
            byte.as_mut_ptr().cast(),
            1,
            libc::MSG_PEEK,
        )
    } <= 0
    {
        return None;
    }
    let (opcode, payload) = peer.frame();
    if opcode == 8 {
        return None;
    }
    assert_eq!(opcode, 1);
    Some(serde_json::from_slice(&payload).unwrap())
}

fn publisher_response(request: &J, coordinator: &Path, cwd: &Path, turns: &mut J) -> J {
    let params = &request["params"];
    match request["method"].as_str().unwrap() {
        "thread/start" => publisher_created(params, coordinator, cwd),
        "thread/read" => json!({"thread":{"id":params["threadId"],"cwd":cwd,"turns":turns,
            "modelProvider":"existing","status":{"type":"idle"},"canAcceptDirectInput":true}}),
        "turn/start" => publisher_blocker(params, turns),
        "thread/delete" => {
            assert!([SESSION, PUBLISHER].contains(&params["threadId"].as_str().unwrap()));
            json!({})
        }
        "thread/list" => json!({"data":[],"nextCursor":null}),
        method => panic!("unexpected publisher fixture RPC: {method}"),
    }
}

fn publisher_created(params: &J, coordinator: &Path, cwd: &Path) -> J {
    let publisher = params["model"] == "publisher-explicit";
    if publisher {
        assert_eq!(params["sandbox"], "read-only");
        assert_eq!(params["dynamicTools"], json!([]));
        assert_eq!(params["config"]["model_reasoning_effort"], "medium");
        let current = util::read_state_json(coordinator, "publication-current.json").unwrap();
        let root = PathBuf::from(current["root"].as_str().unwrap());
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(root.join("server.log"))
            .unwrap()
            .write_all(b"publisher broker log sentinel\n")
            .unwrap();
    }
    json!({"thread":{"id":if publisher {PUBLISHER} else {SESSION},"cwd":cwd,"turns":[],
        "modelProvider":"existing"},"model":params["model"],"modelProvider":"existing",
        "reasoningEffort":if publisher {"medium"} else {"high"}})
}

fn publisher_blocker(params: &J, turns: &mut J) -> J {
    assert_eq!(params["threadId"], PUBLISHER);
    assert_eq!(params["model"], "publisher-explicit");
    assert_eq!(params["effort"], "medium");
    assert_eq!(
        params["sandboxPolicy"],
        json!({"type":"readOnly","networkAccess":false})
    );
    let id = &params["outputSchema"]["properties"]["delivery"]["enum"][0];
    let report = json!({"delivery":id,"action":"report_blocker","facts":"fixture operator review required",
        "next_step":"inspect the original publication; do not push"});
    *turns = json!([{"id":"publisher-blocker","status":"completed",
        "items":[{"type":"agentMessage","text":report.to_string()}]}]);
    json!({"turn":{"id":"publisher-blocker"}})
}

fn bind_coordinator(f: &Fixture, env: &mut task_env::TaskEnvironment, coordinator: &Path) {
    let guard = coordinator.join("guard");
    task_runtime::native::receiver::bootstrap(
        env,
        coordinator,
        &json!({"guard_dir":guard,"task":"task","role":"coordinator"}),
    )
    .unwrap();
    task_runtime::native::receiver::active::activate(
        env,
        coordinator,
        &json!({"driver_binary":env!("CARGO_BIN_EXE_herdr-flow")}),
    )
    .unwrap();
    let receiver = util::read_state_json(&guard, "state.json").unwrap();
    let mut state = f.state();
    state["guard_dir"] = json!(guard);
    state["receiver"] = guard::scope(&receiver);
    queue::save(&f.queue, &mut state).unwrap();
}

fn wait_publisher_signal(coordinator: &Path) -> J {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let status = task_runtime::publication::status(coordinator).unwrap();
        if status["operation"]["phase"] == "signalled" {
            return status;
        }
        assert!(
            Instant::now() < deadline,
            "publisher never signalled: {status}"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn cleanup_publisher(f: &Fixture) {
    upstream_runtime::finalize_with_retry(f);
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let report = queue::finalization::drive(&f.queue).unwrap();
        if report.pending == 0 {
            break;
        }
        assert!(
            report
                .errors
                .iter()
                .all(|error| error.contains("lifecycle action busy")),
            "{report:?}"
        );
        assert!(
            Instant::now() < deadline,
            "publisher cleanup timed out: {report:?}"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(!task_env::is_registered("task").unwrap());
    assert!(f.state()["tasks"].as_object().unwrap().is_empty());
    assert!(f.state()["waits"].as_object().unwrap().is_empty());
    assert!(f.state()["events"].as_object().unwrap().is_empty());
}

#[test]
fn public_start_registers_one_owned_publisher_and_signals_its_blocker_before_cleanup() {
    let f = Fixture::new();
    let mut env = accepted_environment(&f);
    let _path = GhPathGuard::install(&f);
    let coordinator = task_runtime::prepare(&env, "publisher-coordinator").unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let server = publisher_server(
        &coordinator,
        &env.workspace_root.join("checkout"),
        requests.clone(),
    );
    let source = publisher_source(&server.path, &env.workspace_root.join("checkout"));
    let _broker = FixtureChild(task_runtime::launch(&mut env, &coordinator, &source).unwrap());
    bind_coordinator(&f, &mut env, &coordinator);
    let input = json!({"state_version":f.state()["tasks"]["task"]["state_version"],
        "authorization":"deliver","publication_authorization":"publish","merge_authorization":"merge",
        "issue":130,"title":"Reviewed fixture change","body":"Fixture only; no remote writes"});
    let initial = task_runtime::publication::start(&mut env, &coordinator, &input).unwrap();
    let registered = env.child_processes.clone();
    let repeated = task_runtime::publication::start(&mut env, &coordinator, &input).unwrap();
    assert_eq!(initial["id"], repeated["id"]);
    assert_eq!(initial["root"], repeated["root"]);
    assert_eq!(env.child_processes, registered);
    let status = wait_publisher_signal(&coordinator);
    assert_eq!(status["operation"]["result_outcome"], "decision");
    verify_publisher_start(&f, &env, &coordinator, &status);
    let workspace = env.workspace_root.clone();
    let scratch = env.temp_root.clone();
    let instance = env.instance_id.clone();
    drop(env);
    cleanup_publisher(&f);
    assert!(!workspace.exists());
    assert!(!scratch.exists());
    assert!(task_env::marked_processes(&instance).is_empty());
    server.finish();
    let requests = requests.lock().unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|request| request["method"] == "thread/start")
            .count(),
        2
    );
    assert_eq!(
        requests
            .iter()
            .filter(|request| request["method"] == "turn/start")
            .count(),
        1
    );
    assert_eq!(
        requests
            .iter()
            .filter(|request| request["method"] == "thread/delete")
            .count(),
        2
    );
}

fn verify_publisher_start(
    f: &Fixture,
    env: &task_env::TaskEnvironment,
    coordinator: &Path,
    status: &J,
) {
    let root = PathBuf::from(status["root"].as_str().unwrap());
    let runtime = util::read_state_json(&root, "runtime.json").unwrap();
    assert_eq!(
        runtime["model"],
        publisher_source(Path::new("unused"), Path::new("unused"))["role_models"]["publisher"]
    );
    let watch = util::read_state_json(&root, "watch-start.json").unwrap();
    assert_ne!(watch["pid"], runtime["pid"]);
    assert!(env.owns_process(runtime["pid"].as_i64().unwrap() as i32));
    assert!(root.join("publication-watch.log").is_file());
    assert!(root.join("broker.log").is_file());
    assert!(fs::read_to_string(root.join("server.log"))
        .unwrap()
        .contains("publisher broker log sentinel"));
    let result = util::read_state_json(
        &queue::artifacts(&f.queue).unwrap().join("results"),
        &format!("publish-{}.json", status["id"].as_str().unwrap()),
    )
    .unwrap();
    assert_eq!(result["outcome"], "decision");
    let wait = format!("publish-{}", status["id"].as_str().unwrap());
    assert_eq!(f.state()["waits"][wait]["producer"]["session"], PUBLISHER);
    assert_eq!(result["lifecycle_owner"]["task_id"], "task");
    assert_eq!(
        fs::read_to_string(f.temp.path().join("gh-calls"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    let output = Command::new(env!("CARGO_BIN_EXE_herdr-flow"))
        .args([
            "task-runtime",
            "publication-status",
            "--task",
            "task",
            "--state-dir",
        ])
        .arg(coordinator)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<J>(&output.stdout).unwrap()["id"],
        status["id"]
    );
}
