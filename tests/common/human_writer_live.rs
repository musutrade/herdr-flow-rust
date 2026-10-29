//! Explicitly selected real writer transfer. The operator is a script, not a UI user.
//! Setup is the existing stage fixture; business tools and verdicts remain model-owned.
use super::*;
#[path = "human_writer_live_cleanup.rs"]
mod cleanup;
#[path = "human_writer_live_audit.rs"]
mod evidence;

const NOTE: &str =
    "Scripted operator exercised formal human writer ownership; this is not a human UI session.\n";
const HUMAN_GOAL: &str = "Create tracked answer.txt containing exactly 42 followed by one newline, and tracked explanation.txt containing exactly 6 * 7 = 42 followed by one newline. operator-note.txt may initially be absent; if present it must be tracked, contain exactly 'Scripted operator exercised formal human writer ownership; this is not a human UI session.' followed by one newline, and remain in final delivery.";
const READY: &str = "HUMAN-WRITER-READY";

fn setup_human(f: &Fixture, target: &Path) -> task_env::TaskEnvironment {
    setup_agreement(
        f,
        target,
        HUMAN_GOAL,
        json!([
            {"id":"answer","description":"tracked answer.txt has exactly bytes 42 plus newline","required":true},
            {"id":"explanation","description":"tracked explanation.txt has exactly bytes 6 * 7 = 42 plus newline","required":true},
            {"id":"operator-note","description":format!("operator-note.txt may be absent; if present, it must be tracked with exact bytes {NOTE:?} and preserved in final delivery"),"required":true}
        ]),
    )
}

fn instruction(f: &Fixture, target: &Path) -> String {
    format!(
        r#"Complete only registered task task through herdr_queue, using the actual registered full agreement. This is an explicitly selected human writer transfer acceptance. All control operations use product tools, never shell control commands or queue file edits. Use inspect for CURRENT state_version after mutations. Never fabricate developer results, review verdicts, delivery ACKs, or authorization. Every run spec contains wait,instruction,evidence (array, possibly []). A run reply only confirms dispatch: STOP until its real result notice, ACK that exact delivery, inspect, then apply that recorded result. Never repeat an ambiguous run or Git/send operation.

Before human handoff:
1. Register development activity dev, input {seed}, session_epoch 1, result_wait {{"id":"dev","kind":"signal","task":"dev","epoch":1,"assignment":"dev-a1","owner":"developer","reason":"full initial implementation","condition":"report","timeout_seconds":600}}. Run dev to create and commit answer.txt with exactly 42 plus newline and explanation.txt with exactly 6 * 7 = 42 plus newline. Leave operator-note.txt absent. Developer can use git -c user.name=Acceptance -c user.email=acceptance@example.test commit per command. No network, global config, credentials, proxy or shared service changes. You must not edit tracked files yourself.
2. ACK the actual result and apply dev with runtime_results output, top-level acceptance evidence for all three criteria, release_evidence identifying the completed turn, and next_action {{"kind":"complete"}}. Select its full SHA with lineage kind candidate, task task, CURRENT_PARENT_VERSION, candidate and reason.
3. Register independent acceptance activity old-review, input that SHA, role acceptance, session_epoch 1, result_wait {{"id":"old-review","kind":"signal","task":"old-review","epoch":1,"assignment":"old-review-a1","owner":"reviewer","reason":"independent full review before explicit handoff","condition":"report","timeout_seconds":600}}. Run it to inspect the actual fixed tracked bytes against ALL THREE supplied criteria and exact original sources, and write its own .herdr-verdict.json with acceptance_protocol. No tracked edits/commits. The optional operator note is absent in this commit. ACK its real notice and apply its immutable passed verdict ID with next_action {{"kind":"delivery","owner":"coordinator","reason":"initial full review passed; explicit human handoff remains","condition":"existing local delivery authorization after fresh post-handoff review"}}.
4. Inspect approval and then end your turn with exactly HUMAN-WRITER-READY. Do not deliver, finalize, register another activity, edit files or poll. The operator will use formal human-request/acquire, make a documented Git commit, and human-return. Remain idle until the existing Recovery Agent's normal handoff. Human/recovery is not independent acceptance.

Only after the successful Recovery result reaches THIS original session:
5. Inspect task state and human-status via available observation tools. Require released human writer, original requirements unchanged, old candidate and approval invalidated, and advanced acceptance round. Inspect checkout HEAD and the actual operator-note.txt. Register the actual new Git artifact via lineage kind artifact with its full SHA, adapter git and checkout location, then select that SHA as candidate. Do not rewrite the operator's commit or repeat old dev/review operations.
6. Register NEW independent acceptance activity fresh-review, role acceptance, input that new SHA, session_epoch 1, result_wait {{"id":"fresh-review","kind":"signal","task":"fresh-review","epoch":1,"assignment":"fresh-review-a1","owner":"reviewer","reason":"new independent full acceptance after human modification","condition":"report","timeout_seconds":600}}. Run it to inspect ALL THREE original criteria and exact sources, including the newly present operator-note.txt's exact bytes and tracked status. Write its own .herdr-verdict.json with acceptance_protocol; no tracked modifications/commits. ACK its exact notice and apply its OWN immutable passed verdict with delivery next_action, owner coordinator, reason fresh independent full acceptance passed, condition existing local authorization. Do not reuse the old approval.
7. Deliver the new approved SHA using adapter git, target {target}, ref refs/heads/accepted, expected {seed}, authorization deliver-local and CURRENT_PARENT_VERSION. Require confirmed phase and actual target head. Inspect again and finalize task task, outcome success, CURRENT state_version, concrete reason. The normal supervisor reclaims all resources. On blocked/unknown state report the original facts and stop; do not invent a timeout outcome, repair shared services, replay sends, or claim success."#,
        seed = json!(f.commit),
        target = json!(target)
    )
}

fn human_cli(f: &Fixture, coordinator: &Path, action: &str, spec: &J) -> J {
    let path = f.temp.path().join(format!("{action}-spec.json"));
    util::atomic_write_json(&path, spec).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_herdr-flow"))
        .args(["task-runtime", action, "--task", "task", "--state-dir"])
        .arg(coordinator)
        .arg("--spec")
        .arg(path)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "formal {action} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let result = serde_json::from_slice(&out.stdout).unwrap();
    util::atomic_write_json(
        &f.temp.path().join(format!("{action}-result.json")),
        &result,
    )
    .unwrap();
    result
}

#[test]
#[ignore = "explicit real service/models/Herdr and cleanup inventory configuration; scripted human operator"]
fn real_human_writer_return_reaccepts_delivers_and_reclaims() {
    let config = evidence::Configuration::load(); // Validate everything before task allocation.
    let mut f = Fixture::new();
    f.temp.disable_cleanup(true);
    eprintln!(
        "human writer live retained root: {}",
        f.temp.path().display()
    );
    let target = f.temp.path().join("output.git");
    let mut env = setup_human(&f, &target);
    let coordinator = task_runtime::prepare(&env, "coordinator").unwrap();
    let mut audit = evidence::Audit::new(&f, &env, &config);
    audit.persist(&f);
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let launch = config.launch(&env.workspace_root);
        let mut broker = task_runtime::launch(&mut env, &coordinator, &launch).unwrap();
        let guard_root = coordinator.join("guard");
        task_runtime::native::receiver::bootstrap(
            &env,
            &coordinator,
            &json!({"guard_dir":guard_root,"task":"task","role":"coordinator"}),
        )
        .unwrap();
        let activated = task_runtime::native::receiver::active::activate(
            &mut env,
            &coordinator,
            &json!({"driver_binary":env!("CARGO_BIN_EXE_herdr-flow")}),
        )
        .unwrap();
        assert_eq!(activated["recovery"]["agent"]["phase"], "ready");
        bind(&f, &guard_root);
        let started = task_runtime::native::receiver::start(
            &env,
            &coordinator,
            &json!({"instruction":instruction(&f,&target)}),
        )
        .unwrap();
        assert_eq!(started["presentation"]["phase"], "ready");
        audit.start = started;
        // Release the existing environment lock before model tool operations.
        drop(env);
        let mut supervisor = watch(&f);
        let deadline = Instant::now() + Duration::from_secs(1200);
        loop {
            audit.observe(&f, &config);
            if audit.ready(&f, &config) {
                break;
            }
            audit.bound(&mut supervisor, deadline);
        }
        audit.capture_original(&f, &config);
        let request = json!({"id":"scripted-operator","actor":"acceptance-script",
            "reason":"Explicitly authorized scripted operator adds the pre-agreed optional tracked operator note; not a human UI interaction"});
        let requested = human_cli(&f, &coordinator, "human-request", &request);
        assert_eq!(requested["phase"], "draining");
        let bound = json!({"id":request["id"],"actor":request["actor"]});
        audit.acquire = human_cli(&f, &coordinator, "human-acquire", &bound);
        audit.persist(&f);
        audit.assert_acquired(&f);
        let checkout = audit.workspace.join("checkout");
        fs::write(checkout.join("operator-note.txt"), NOTE).unwrap();
        git(&checkout, &["add", "operator-note.txt"]);
        git(
            &checkout,
            &[
                "-c",
                "user.name=Scripted Operator",
                "-c",
                "user.email=operator@example.test",
                "commit",
                "-m",
                "Record explicitly acquired scripted operator handoff",
            ],
        );
        audit.human_sha = json!(git(&checkout, &["rev-parse", "HEAD"]));
        audit.returned = human_cli(
            &f,
            &coordinator,
            "human-return",
            &json!({
            "id":bound["id"],"actor":bound["actor"],"note":format!("Scripted operator, not a human UI session, acquired formal phase=human and committed {} adding only the pre-agreed operator-note.txt. All commands exited; no writes after return. Obtain fresh independent full acceptance before delivery.",audit.human_sha)}),
        );
        assert_eq!(audit.returned["phase"], "verifying");
        audit.persist(&f);
        loop {
            audit.observe(&f, &config);
            if f.state()["tasks"]["task"].is_null() {
                break;
            }
            audit.bound(&mut supervisor, deadline);
        }
        supervisor.0.kill().unwrap();
        supervisor.0.wait().unwrap();
        while broker.try_wait().unwrap().is_none() {
            assert!(
                Instant::now() < deadline,
                "broker exit observation limit; preserve original task"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
        audit.verify(&f, &config, &target);
    }));
    if outcome.is_err() {
        // Do not finalize a failed/unknown experiment or erase its original task.
        audit.persist(&f);
        util::atomic_write_json(&f.temp.path().join("human-live-failure.json"), &json!({
            "status":"failed; original result retained", "owner":audit.owner,"root":f.temp.path(),
            "error":outcome.as_ref().err().map(|error| error.downcast_ref::<String>().cloned().or_else(||error.downcast_ref::<&str>().map(|s|(*s).to_owned())).unwrap_or_else(||"non-text panic; inspect original test output".into())),
            "human_status":{"program":env!("CARGO_BIN_EXE_herdr-flow"),"args":["task-runtime","human-status","--task","task","--state-dir",coordinator],"env":{"HERDR_TASK_ENV_ROOT":f.queue.join("queue.json/task-environments")}},
            "recovery_status":{"program":env!("CARGO_BIN_EXE_herdr-flow"),"args":["task-runtime","recovery-status","--task","task","--state-dir",coordinator],"env":{"HERDR_TASK_ENV_ROOT":f.queue.join("queue.json/task-environments")}},
            "coordinator":coordinator,"queue":f.queue,"registry":f.queue.join("queue.json/task-environments"),
            "next_step":"Read human-live-audit.json and original protected runtime/queue records. Use formal task-runtime human-status and recovery-status for this exact coordinator/task. The queue watcher may need its original observation-only restart. Never rerun receiver-start, original business operations, or this test on the retained root."})).unwrap();
    }
    if let Err(error) = outcome {
        std::panic::resume_unwind(error);
    }
}

#[test]
fn human_fixture_predeclares_optional_edit_without_preseeded_approval() {
    let f = Fixture::new();
    let target = f.temp.path().join("output.git");
    let env = setup_human(&f, &target);
    let q = f.state();
    let task = &q["tasks"]["task"];
    assert_eq!(task["requirements"]["1"]["goal"], HUMAN_GOAL);
    assert_eq!(
        task["requirements"]["1"]["criteria"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    assert!(task["activities"].as_object().unwrap().is_empty());
    assert!(task["approved"].is_null() && task["human_writer"].is_null());
    assert!(!env
        .workspace_root
        .join("checkout/operator-note.txt")
        .exists());
    drop(env);
    upstream_runtime::finalize_with_retry(&f);
    assert_eq!(queue::finalization::drive(&f.queue).unwrap().pending, 0);
}
