//! Opt-in real coordinator, development, independent review and Herdr acceptance.
//! The harness creates the agreement; all business mutations use model tools.
use super::*;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};
#[path = "stage_live_audit.rs"]
mod audit;
#[path = "stage_live_resume.rs"]
mod resume;

const GOAL: &str = "Create tracked answer.txt containing exactly 42 followed by one newline, and tracked explanation.txt containing exactly 6 * 7 = 42 followed by one newline.";

fn herdr(args: &[&str]) -> J {
    let output = Command::new("herdr").args(args).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice::<J>(&output.stdout).unwrap()["result"].clone()
}

fn mutate(q: &mut J, mut spec: J) {
    spec["task"] = json!("task");
    spec["state_version"] = q["tasks"]["task"]["state_version"].clone();
    progress::lineage::action(q, &spec, util::now()).unwrap();
}

fn setup(f: &Fixture, target: &Path) -> task_env::TaskEnvironment {
    git(
        f.temp.path(),
        &[
            "clone",
            "--bare",
            f.temp.path().join("repo").to_str().unwrap(),
            target.to_str().unwrap(),
        ],
    );
    git(target, &["update-ref", "refs/heads/accepted", &f.commit]);
    let mut q = f.state();
    q["tasks"].as_object_mut().unwrap().remove("task");
    progress::lineage::action(&mut q, &json!({"kind":"plan","task":"task","goal":GOAL,
        "basis":{"original":[GOAL],"changes":[],"specifications":[],"endpoint":target,"confirmation":"explicit stage-delivery acceptance scenario"},
        "acceptance":[{"id":"answer","description":"tracked answer.txt has exactly bytes 42 plus newline","required":true},
            {"id":"explanation","description":"tracked explanation.txt has exactly bytes 6 * 7 = 42 plus newline","required":true}],
        "authorization":[{"id":"develop","action":"development","source":"scoped local acceptance task"},
            {"id":"verify","action":"review","source":"scoped local acceptance task"},
            {"id":"deliver-local","action":"delivery","source":"only this newly created local Git target"}]}), util::now()).unwrap();
    mutate(
        &mut q,
        json!({"kind":"artifact","artifact":{"id":f.commit,"adapter":"git","location":f.temp.path().join("repo")}}),
    );
    mutate(
        &mut q,
        json!({"kind":"candidate","candidate":f.commit,"reason":"empty seed required for isolated environment admission"}),
    );
    queue::save(&f.queue, &mut q).unwrap();
    let mut create = f.create_spec();
    create["environment"]["delivery_targets"] = json!([{"path":target,"target_type":"git","description":"private stage acceptance output"}]);
    f.call(&create).unwrap();
    task_env::restore_environment("task").unwrap()
}

fn bind(f: &Fixture, guard_root: &Path) {
    let _lock = queue::locked(&f.queue, false).unwrap();
    let state = util::read_state_json(guard_root, "state.json").unwrap();
    let mut q = f.state();
    q["guard_dir"] = json!(guard_root);
    q["receiver"] = guard::scope(&state);
    q["notify"] = json!(true);
    q["poll_seconds"] = json!(1);
    queue::save(&f.queue, &mut q).unwrap();
}

fn instruction(f: &Fixture, target: &Path) -> String {
    format!(
        r#"Complete only registered task task through herdr_queue. This is real stage-delivery acceptance: use real developer and independent reviewer results, never fabricate them. The initial artifact is commit {seed}. The predeclared local Git target is {target}. All control operations use tools, not shell commands or queue file edits. Use inspect for CURRENT parent/child state_version after mutations. Every run spec contains wait,instruction,evidence (an array, possibly []). A run reply confirms dispatch only: stop until a result notification, ACK its exact delivery, then inspect and apply the recorded result. Use new activities for subsequent work; never repeat an ambiguous run or delivery.

1. Inspect the agreement and existing authority. Start stage with lineage spec {{"kind":"stage","task":"task","state_version":CURRENT_PARENT_VERSION,"id":"answer-stage","scope":"Deliver the independently reviewed answer file; explanation remains for final acceptance","criteria":["answer"]}}.
2. Register development activity dev with input {seed}, session_epoch 1 and result_wait {{"id":"dev","kind":"signal","task":"dev","epoch":1,"assignment":"dev-a1","owner":"developer","reason":"stage answer development","condition":"report","timeout_seconds":600}}. Run dev to create and commit answer.txt containing exactly 42 plus newline. For this first stage it MUST leave explanation.txt absent. Developer may use git -c user.name=Acceptance -c user.email=acceptance@example.test commit, per command only. No network, account or global configuration changes. You must not edit the artifact yourself.
3. ACK the result. Apply dev with output from runtime_results, acceptance {{"answer":"evidence from the collected completed development turn"}}, release_evidence identifying that turn, and next_action {{"kind":"complete"}}; these are top-level apply fields. Select its full SHA with lineage kind candidate, task task, CURRENT_PARENT_VERSION, candidate and reason. Register independent acceptance activity stage-review, role acceptance, input that SHA, session_epoch 1, result_wait {{"id":"stage-review","kind":"signal","task":"stage-review","epoch":1,"assignment":"stage-review-a1","owner":"reviewer","reason":"independent answer-stage review","condition":"report","timeout_seconds":600}}. Run it to inspect the actual fixed tracked bytes against its supplied stage requirement and write .herdr-verdict.json using acceptance_protocol. It must use the exact supplied original source and criterion answer, make no tracked edits or commits, and reach its own conclusion. ACK and apply its immutable verdict ID; passed uses next_action {{"kind":"delivery","owner":"coordinator","reason":"stage independently passed","condition":"existing local delivery authorization"}}. Never supply independent reviewer evidence yourself.
4. Deliver the accepted stage SHA with spec {{"adapter":"git","target":{target},"ref":"refs/heads/accepted","expected":{seed},"authorization":"deliver-local","state_version":CURRENT_PARENT_VERSION}}. Require phase confirmed. Do NOT finalize this stage. Use stage-complete with spec {{"delivery":"EXACT_CONFIRMED_INTENT_ID"}} from the actual reply or inspected parent delivery. Inspect: the same parent should be running, with no current stage, candidate or approval, and stage_delivery.phase complete. If resource_cleanup.phase is pending, STOP and await the original host stage cleanup notice. ACK that exact host notice WITHOUT task-apply, then inspect the parent and require resource_cleanup.phase complete before any next lineage activity or run. Never poll or replay stage delivery/cleanup. A retained or unknown cleanup is a blocker to report, not permission to continue. Save stage_delivery.delivery.artifact.id as STAGE_SHA. Never substitute stage completion for the final requirement.
5. Register NEW development activity final-dev, role development, input STAGE_SHA, session_epoch 1, result_wait {{"id":"final-dev","kind":"signal","task":"final-dev","epoch":1,"assignment":"final-dev-a1","owner":"developer","reason":"complete remaining agreed requirement","condition":"report","timeout_seconds":600}}. Run it to retain answer.txt exactly 42 plus newline, add explanation.txt exactly 6 * 7 = 42 plus newline, and commit the result. ACK/apply its actual output with acceptance evidence for BOTH answer and explanation, release_evidence and next_action complete. Select the new candidate.
6. Register NEW independent acceptance activity final-review with role acceptance, the new SHA, session_epoch 1, result_wait {{"id":"final-review","kind":"signal","task":"final-review","epoch":1,"assignment":"final-review-a1","owner":"reviewer","reason":"independent overall acceptance","condition":"report","timeout_seconds":600}}. Run it to inspect BOTH exact tracked files against the full supplied requirement, writing its own report with both criterion IDs and exact original sources. No tracked edits or commits. ACK/apply its own immutable passed verdict with the same delivery next_action shape. Do not reuse the stage approval.
7. Deliver using adapter git, the SAME predeclared target and refs/heads/accepted, expected STAGE_SHA, authorization deliver-local and CURRENT_PARENT_VERSION. Only after confirmed non-stage delivery, inspect again and finalize task task with outcome success, current state_version and concrete reason. The existing supervisor will reclaim the task. On a real defect report it honestly through normal repair/re-review rules; on unknown external outcomes stop and report without replay. Never invent authorization, alter the agreement, finalize early or claim a failed/unknown delivery succeeded."#,
        seed = json!(f.commit),
        target = json!(target)
    )
}

fn watch(f: &Fixture) -> FixtureChild {
    let log = fs::File::create(f.temp.path().join("stage-watch.log")).unwrap();
    FixtureChild(
        Command::new(env!("CARGO_BIN_EXE_herdr-flow"))
            .args(["queue", "watch", "--state-dir"])
            .arg(&f.queue)
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap(),
    )
}

fn upstream_empty(socket: &str, cwds: &[PathBuf]) {
    let mut client = herdr_flow::codex::rpc::Client::new(socket);
    client.timeout = 5.0;
    for archived in [false, true] {
        let reply = client.call("thread/list", &json!({"cwd":cwds,"archived":archived,"useStateDbOnly":true,"limit":128,
            "sourceKinds":["cli","vscode","exec","appServer","subAgent","subAgentReview","subAgentCompact","subAgentThreadSpawn","subAgentOther","unknown"]})).unwrap();
        assert!(
            reply["data"].as_array().unwrap().is_empty() && reply["nextCursor"].is_null(),
            "owned native sessions remain"
        );
    }
}

#[test]
#[ignore = "explicit existing model service and Herdr; real stage delivery, fresh overall acceptance and cleanup"]
fn real_stage_delivery_continues_same_parent_and_reclaims_visible_roles() {
    // Resolve and validate before allocating any task-owned state.
    let endpoint = resume::socket(Path::new(
        &std::env::var("HERDR_UPSTREAM_SOCKET").expect("existing service required"),
    ));
    let socket = endpoint["path"].as_str().unwrap().to_owned();
    let model: J = serde_json::from_str(&std::env::var("HERDR_UPSTREAM_MODEL").unwrap()).unwrap();
    let roles: J =
        serde_json::from_str(&std::env::var("HERDR_UPSTREAM_ROLE_MODELS").unwrap()).unwrap();
    let binary = std::env::var("HERDR_NATIVE_BINARY").unwrap();
    let store = PathBuf::from(std::env::var("HERDR_SESSION_STORE").unwrap());
    let config = PathBuf::from(std::env::var("HERDR_UPSTREAM_CONFIG").unwrap());
    let config_before = fs::read(&config).unwrap();
    let parent = std::env::var("HERDR_PANE_ID").unwrap();
    let parent_before = herdr(&["agent", "get", &parent]);
    let resume_spec: Option<J> = std::env::var_os("HERDR_STAGE_RESUME").map(|value| {
        serde_json::from_str(value.to_str().expect("resume contract must be UTF-8")).unwrap()
    });
    let mut f = match &resume_spec {
        Some(spec) => resume::attach(Path::new(spec["root"].as_str().unwrap())),
        None => Fixture::new(),
    };
    f.temp.disable_cleanup(true); // Failed/unknown tasks retain their original state.
    eprintln!("stage live root: {}", f.temp.path().display());
    let started_at = Instant::now();
    let target = f.temp.path().join("output.git");
    let current = json!({"model":model,"role_models":roles,"parent":parent_before,"binary":binary,
        "socket_identity":endpoint,"config_sha256":util::pyjson::sha256_hex(&config_before)});
    let resumed = resume_spec
        .as_ref()
        .map(|spec| resume::validate(&f, spec, &current));
    let mut env = if resumed.is_some() {
        task_env::restore_environment("task").unwrap()
    } else {
        setup(&f, &target)
    };
    let workspace = env.workspace_root.clone();
    let scratch = env.temp_root.clone();
    let coord = task_runtime::prepare(&env, "coordinator").unwrap();
    let mut evidence = audit::StageAudit::new(
        &f,
        &env,
        &target,
        &model,
        &roles,
        &parent_before,
        (&socket, &store),
    );
    if let Some(receipt) = resumed {
        util::atomic_write_json(&f.temp.path().join("stage-resume.json"), &receipt).unwrap();
    } else {
        util::atomic_write_json(&f.temp.path().join("stage-baseline.json"), &json!({
        "owner":lifecycle::task_owner(&f.state(),"task").unwrap(),"agreement":f.state()["tasks"]["task"]["requirements"],
        "model":model,"role_models":roles,"parent":parent_before,
        "config_sha256":util::pyjson::sha256_hex(&config_before),"binary":binary,"socket":socket
    })).unwrap();
    }
    let mut broker = task_runtime::launch(&mut env, &coord, &json!({"adapter":"codex","configuration":"upstream",
        "socket":socket,"cwd":workspace.join("checkout"),"model":model,"role_models":roles,"approval_policy":"on-request",
        "driver_binary":env!("CARGO_BIN_EXE_herdr-flow"),"native_tui":{"binary":binary,"parent_pane":parent,"session_store":store}})).unwrap();
    let guard_root = coord.join("guard");
    task_runtime::native::receiver::bootstrap(
        &env,
        &coord,
        &json!({"guard_dir":guard_root,"task":"task","role":"coordinator"}),
    )
    .unwrap();
    task_runtime::native::receiver::active::activate(
        &mut env,
        &coord,
        &json!({"driver_binary":env!("CARGO_BIN_EXE_herdr-flow")}),
    )
    .unwrap();
    bind(&f, &guard_root);
    let started = task_runtime::native::receiver::start(
        &env,
        &coord,
        &json!({"instruction":instruction(&f,&target)}),
    )
    .unwrap();
    util::atomic_write_json(&f.temp.path().join("stage-start.json"), &started).unwrap();
    assert_eq!(started["presentation"]["phase"], "ready", "{started}");
    drop(env);
    let mut supervisor = watch(&f);
    // A test observation bound only; it does not terminate/replay business work.
    let deadline = Instant::now() + Duration::from_secs(900);
    loop {
        let q = f.state();
        evidence.observe(&f, &q);
        if q["tasks"]["task"].is_null() {
            break;
        }
        assert!(
            supervisor.0.try_wait().unwrap().is_none(),
            "supervisor exited; preserve original task"
        );
        assert!(
            Instant::now() < deadline,
            "acceptance observation bound; preserve original task without replay"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    supervisor.0.kill().unwrap();
    supervisor.0.wait().unwrap();
    broker.wait().unwrap();
    evidence.verify(&f, &target, &store);
    upstream_empty(
        &socket,
        &[
            workspace.join("checkout"),
            workspace.join("reviews/stage-review-a1"),
            workspace.join("reviews/final-review-a1"),
        ],
    );
    assert!(!workspace.exists() && !scratch.exists());
    assert!(!task_env::is_registered("task").unwrap());
    assert!(!git(
        &f.temp.path().join("repo"),
        &["worktree", "list", "--porcelain"]
    )
    .contains("workspace/checkout"));
    assert!(
        fs::read(&config).unwrap() == config_before,
        "upstream configuration changed"
    );
    let after = herdr(&["agent", "get", &parent]);
    for field in ["terminal_id", "workspace_id", "tab_id", "agent_session"] {
        assert_eq!(
            after["agent"][field], parent_before["agent"][field],
            "parent {field} changed"
        );
    }
    let mut summary = evidence.summary();
    summary["seconds"] = json!(started_at.elapsed().as_secs_f64());
    summary["cleanup"] = json!({"roots":true,"registration":true,"worktree":true,"native_sessions":true,"parent_unchanged":true,"config_unchanged":true});
    util::atomic_write_json(&f.temp.path().join("stage-result.json"), &summary).unwrap();
    eprintln!(
        "stage live passed in {:.2}s",
        started_at.elapsed().as_secs_f64()
    );
}

#[test]
fn stage_fixture_has_full_agreement_but_no_preseeded_stage_activity_or_verdict() {
    let f = Fixture::new();
    let target = f.temp.path().join("output.git");
    let env = setup(&f, &target);
    let q = f.state();
    assert_eq!(
        q["tasks"]["task"]["requirements"]["1"]["criteria"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert!(q["tasks"]["task"]["activities"]
        .as_object()
        .unwrap()
        .is_empty());
    assert!(q["tasks"]["task"]["stage"].is_null());
    assert!(q["tasks"]["task"]["approved"].is_null());
    assert_eq!(
        git(&target, &["rev-parse", "refs/heads/accepted"]),
        f.commit
    );
    drop(env);
    upstream_runtime::finalize_with_retry(&f);
    assert_eq!(queue::finalization::drive(&f.queue).unwrap().pending, 0);
}
