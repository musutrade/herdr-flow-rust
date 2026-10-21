//! Real native role execution; coordinator/notification receiver is a test harness.
use super::*;
use herdr_flow::{herdr::receiver_factory, util};
fn change(q: &Path, mut spec: J) -> J {
    let mut s = queue::load_queue(q).unwrap();
    spec["task"] = json!("project");
    spec["state_version"] = s["tasks"]["project"]["state_version"].clone();
    let result = progress::lineage::action(&mut s, &spec, util::now()).unwrap();
    queue::save(q, &mut s).unwrap();
    result
}
fn activity(name: &str, role: &str, input: &J) -> J {
    json!({"activity":name,"role":role,"input":input,"session_epoch":1,
        "result_wait":{"id":name,"kind":"signal","epoch":1,"assignment":name,"owner":role,"reason":"native role result","condition":"terminal report","timeout_seconds":300}})
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

pub(super) fn native(
    env: &mut TaskEnvironment,
    q: &Path,
    cwd: &Path,
    name: &str,
    instruction: &str,
) -> (J, J) {
    let root = task_runtime::prepare(env, name).unwrap();
    common::create_dir_all(root.join("config/opencode")).unwrap();
    fs::copy(
        std::env::var("HERDR_LIVE_OPENCODE_CONFIG").unwrap(),
        root.join("config/opencode/opencode.json"),
    )
    .unwrap();
    // This bounded test measures four explicit roles, without internal delegation.
    // Only the disposable config copy is changed; native approvals remain native.
    let config_path = root.join("config/opencode/opencode.json");
    let mut config = util::read_json(&config_path).unwrap();
    config["tools"]["task"] = json!(false);
    fs::write(&config_path, serde_json::to_vec(&config).unwrap()).unwrap();
    let model: J =
        serde_json::from_str(&std::env::var("HERDR_LIVE_OPENCODE_MODEL").unwrap()).unwrap();
    let spec = json!({"adapter":"opencode","binary":std::env::var("HERDR_LIVE_OPENCODE_BINARY").unwrap(),"cwd":cwd,"model":model,"port":port()});
    let mut child = task_runtime::launch(env, &root, &spec).unwrap();
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
    retry_unstarted(&root, "dispatch.json", || {
        task_runtime::native::dispatch(env, &root)
    })
    .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(180);
    let result = loop {
        let collected = task_runtime::native::collect(env, &root);
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
                fs::read_to_string(cwd.join(".herdr-verdict.json")).unwrap_or_default()
            )
        });
        if r["phase"] == "signalled" {
            break r;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "native role {name} unresolved: {}",
            r["phase"]
        );
        std::thread::sleep(std::time::Duration::from_millis(500));
    };
    assert_eq!(result["outcome"], "complete");
    child.kill().unwrap();
    child.wait().unwrap();
    println!("real native {name}: exact session collected and server stopped");
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
fn clone_snapshot(env: &TaskEnvironment, artifact: &J, name: &str) -> std::path::PathBuf {
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
