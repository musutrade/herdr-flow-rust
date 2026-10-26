//! Real local file/Git endpoints, with explicit protocol fixtures for review.
use herdr_flow::{
    delivery::{self, Executor, Observation},
    model::J,
    progress::{self, acceptance, lineage},
    queue,
    task_env::*,
    util,
};
use serde_json::json;
use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::{Mutex, MutexGuard},
};
static LOCK: Mutex<()> = Mutex::new(());
struct Fixture {
    _lock: MutexGuard<'static, ()>,
    dir: tempfile::TempDir,
    env: TaskEnvironment,
    queue: PathBuf,
    spec: J,
}
fn mutate(s: &mut J, mut v: J) {
    v["task"] = json!("job");
    v["state_version"] = s["tasks"]["job"]["state_version"].clone();
    lineage::action(s, &v, 1.0).unwrap();
}
fn accepted(artifact: J) -> J {
    let mut s = json!({"id":"q","tasks":{},"waits":{},"events":{},"deliveries":{},"stopped":false});
    lineage::action(&mut s,&json!({"kind":"plan","task":"job","goal":"deliver A","basis":{"original":["request"],"changes":[],"specifications":[],"endpoint":"customer endpoint","confirmation":"user"},"authorization":[{"id":"publish","action":"delivery","source":"user request"}]}),1.0).unwrap();
    mutate(&mut s, json!({"kind":"artifact","artifact":artifact}));
    mutate(
        &mut s,
        json!({"kind":"candidate","candidate":artifact["id"],"reason":"review"}),
    );
    mutate(
        &mut s,
        json!({"kind":"activity","activity":"review","role":"acceptance","input":artifact["id"],"session_epoch":1,"result_wait":{"id":"review","epoch":1,"assignment":"review","owner":"reviewer","kind":"signal","timeout_seconds":30,"reason":"review result","condition":"done","producer":{"adapter":"codex","session":"independent"}}}),
    );
    let verdict=acceptance::submit(&mut s,"review",&json!({"adapter":"codex","session":"independent"}),&json!({"conclusion":"passed","scope":"A","checks":[{"id":"content","criterion":"goal","source":"request","status":"passed","evidence":"checked A"}],"gaps":[],"reuses":[],"resolutions":[]})).unwrap();
    let mut e = json!({"id":"result","task":"review","wait":"review","acknowledged":true,"outcome":"complete"});
    let w = s["waits"]["review"].clone();
    progress::attach(&mut s, &mut e, &w, 2.0).unwrap();
    s["events"]["result"] = e;
    let spec = json!({"task":"review","state_version":s["tasks"]["review"]["state_version"],"event":"result","verdict":verdict["id"],"next_action":{"kind":"delivery","owner":"customer","reason":"passed","condition":"correct endpoint"}});
    progress::apply(&mut s, &spec, 3.0).unwrap();
    s
}
impl Fixture {
    fn new(git: bool) -> Self {
        let lock = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::Builder::new()
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        std::env::set_var("HERDR_TASK_ENV_ROOT", dir.path().join("control"));
        let target = dir.path().join("customer");
        let env = create_environment(
            "job",
            &dir.path().join("work"),
            vec![DeliveryTarget::new(target.clone(), "formal".into())],
            &dir.path().join("tmp"),
        )
        .unwrap();
        let mut spec = json!({"adapter":"file","target":target,"authorization":"publish"});
        let artifact = if git {
            git_cmd(&env.workspace_root, &["init", "-b", "main"]);
            git_cmd(
                &env.workspace_root,
                &["config", "user.email", "fixture@example.test"],
            );
            git_cmd(&env.workspace_root, &["config", "user.name", "Fixture"]);
            std::fs::write(env.workspace_root.join("A"), "base").unwrap();
            git_cmd(&env.workspace_root, &["add", "A"]);
            git_cmd(&env.workspace_root, &["commit", "-m", "base"]);
            let base = git_cmd(&env.workspace_root, &["rev-parse", "HEAD"]);
            git_cmd(
                dir.path(),
                &[
                    "clone",
                    "--bare",
                    env.workspace_root.to_str().unwrap(),
                    target.to_str().unwrap(),
                ],
            );
            std::fs::write(env.workspace_root.join("A"), "A").unwrap();
            git_cmd(&env.workspace_root, &["commit", "-am", "A"]);
            let head = git_cmd(&env.workspace_root, &["rev-parse", "HEAD"]);
            spec["adapter"] = json!("git");
            spec["ref"] = json!("refs/heads/main");
            spec["expected"] = json!(base);
            json!({"adapter":"git","id":head,"location":env.workspace_root})
        } else {
            let source = env.workspace_root.join("A");
            std::fs::write(&source, "A").unwrap();
            json!({"adapter":"file-sha256","id":util::pyjson::sha256_hex(b"A"),"location":source})
        };
        let root = dir.path().join("shared-queue");
        let mut state = accepted(artifact);
        spec["state_version"] = state["tasks"]["job"]["state_version"].clone();
        state["tasks"]["other"] = json!({"task":"other","keep":"untouched"});
        state["events"]["other-event"] = json!({"id":"other-event","task":"other"});
        state["deliveries"]["mixed"] =
            json!({"id":"mixed","events":["result","other-event"],"phase":"acked"});
        state["deliveries"]["only"] = json!({"id":"only","events":["result"],"phase":"acked"});
        queue::save(&root, &mut state).unwrap();
        for id in ["mixed", "only"] {
            util::atomic_write_json(&root.join("deliveries").join(format!("{id}.json")),&json!({"delivery":state["deliveries"][id],"events":[state["events"]["result"],state["events"]["other-event"]]})).unwrap();
        }
        util::atomic_write_json(&root.join("results/review.json"), &json!({"task":"review"}))
            .unwrap();
        Self {
            _lock: lock,
            dir,
            env,
            queue: root,
            spec,
        }
    }
    fn prepare(&mut self) {
        delivery::prepare(
            &mut self.env,
            &self.queue,
            &self.spec,
            &delivery::local::Local,
        )
        .unwrap();
    }
    fn clean(&self) {
        assert!(!self.env.workspace_root.exists());
        assert!(!self.env.temp_root.exists());
        assert!(!is_registered("job").unwrap());
        assert_eq!(occupied_slots().unwrap(), 0);
        for p in ["envs/job.json", "plans/job.json", "locks/job.lock"] {
            assert!(!self.dir.path().join("control").join(p).exists(), "{p}");
        }
        let s = queue::load_queue(&self.queue).unwrap();
        assert!(s["tasks"]["job"].is_null());
        assert!(s["tasks"]["review"].is_null());
        assert_eq!(s["tasks"]["other"]["keep"], "untouched");
        assert_eq!(s["deliveries"]["mixed"]["events"], json!(["other-event"]));
        assert!(!self.queue.join("results/review.json").exists());
        assert!(!self.queue.join("deliveries/only.json").exists());
        assert!(
            !std::fs::read_to_string(self.queue.join("deliveries/mixed.json"))
                .unwrap()
                .contains("review")
        );
    }
}
fn git_cmd(root: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
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
#[test]
fn file_delivery_removes_task_data_and_keeps_formal_output() {
    let mut f = Fixture::new(false);
    f.prepare();
    assert_eq!(
        delivery::execute(&mut f.env, &delivery::local::Local).unwrap()["phase"],
        "confirmed"
    );
    assert!(delivery::execute(&mut f.env, &delivery::local::Local).is_err());
    delivery::finish(&mut f.env).unwrap();
    f.clean();
    assert_eq!(
        std::fs::read_to_string(f.dir.path().join("customer")).unwrap(),
        "A"
    );
}
#[test]
fn git_publishes_only_the_accepted_commit_even_if_head_moves() {
    let mut f = Fixture::new(true);
    f.prepare();
    let accepted = f.env.delivery["artifact"]["id"].clone();
    std::fs::write(f.env.workspace_root.join("A"), "B").unwrap();
    git_cmd(&f.env.workspace_root, &["commit", "-am", "B"]);
    assert_eq!(
        delivery::execute(&mut f.env, &delivery::local::Local).unwrap()["phase"],
        "confirmed"
    );
    assert_eq!(
        json!(git_cmd(
            &f.dir.path().join("customer"),
            &["rev-parse", "main"]
        )),
        accepted
    );
    delivery::finish(&mut f.env).unwrap();
    f.clean();
}
struct Uncertain;
impl Executor for Uncertain {
    fn preflight(&self, _: &J) -> herdr_flow::Result<()> {
        Ok(())
    }
    fn execute(&self, _: &J) -> herdr_flow::Result<()> {
        Err(herdr_flow::FlowError::progress("transport lost"))
    }
    fn observe(&self, _: &J) -> herdr_flow::Result<Observation> {
        Ok(Observation::Unknown("inspect".into()))
    }
}
#[test]
fn unknown_never_replays_and_restart_queries_real_endpoint() {
    let mut f = Fixture::new(false);
    f.prepare();
    assert_eq!(
        delivery::execute(&mut f.env, &Uncertain).unwrap()["phase"],
        "unknown"
    );
    assert!(delivery::finish(&mut f.env).is_err());
    assert!(delivery::execute(&mut f.env, &delivery::local::Local).is_err());
    std::fs::write(f.dir.path().join("customer"), "A").unwrap();
    f.env.release_ownership();
    f.env = restore_environment("job").unwrap();
    assert_eq!(
        delivery::reconcile(&mut f.env, &delivery::local::Local).unwrap()["phase"],
        "confirmed"
    );
    delivery::finish(&mut f.env).unwrap();
    f.clean();
}
#[test]
fn failed_delivery_requires_observation_before_retry() {
    let mut f = Fixture::new(false);
    f.prepare();
    std::fs::write(f.dir.path().join("customer"), "wrong").unwrap();
    assert_eq!(
        delivery::execute(&mut f.env, &delivery::local::Local).unwrap()["phase"],
        "failed"
    );
    assert!(delivery::finish(&mut f.env).is_err());
    assert!(delivery::retry(&mut f.env, &delivery::local::Local).is_err());
    std::fs::remove_file(f.dir.path().join("customer")).unwrap();
    assert_eq!(
        delivery::reconcile(&mut f.env, &delivery::local::Local).unwrap()["phase"],
        "absent"
    );
    assert_eq!(
        delivery::retry(&mut f.env, &delivery::local::Local).unwrap()["phase"],
        "confirmed"
    );
    delivery::finish(&mut f.env).unwrap();
    f.clean();
}
#[test]
fn cleanup_failure_keeps_slot_and_resumes_without_delivery() {
    let mut f = Fixture::new(false);
    f.prepare();
    delivery::execute(&mut f.env, &delivery::local::Local).unwrap();
    let renamed = f.dir.path().join("displaced");
    std::fs::rename(&f.env.workspace_root, &renamed).unwrap();
    assert!(delivery::finish(&mut f.env).is_err());
    assert!(occupied_slots().unwrap() > 0);
    assert_eq!(f.env.delivery["phase"], "cleaning");
    assert!(delivery::execute(&mut f.env, &delivery::local::Local).is_err());
    std::fs::rename(&renamed, &f.env.workspace_root).unwrap();
    f.env.release_ownership();
    f.env = restore_environment("job").unwrap();
    delivery::finish(&mut f.env).unwrap();
    f.clean();
}
#[test]
fn rejects_stale_approval_missing_authorization_and_unregistered_target() {
    let mut f = Fixture::new(false);
    for key in ["authorization", "target", "state_version"] {
        let mut bad = f.spec.clone();
        bad[key] = json!("wrong");
        assert!(delivery::prepare(&mut f.env, &f.queue, &bad, &delivery::local::Local).is_err());
        assert!(f.env.delivery.is_null());
    }
    let mut s = queue::load_queue(&f.queue).unwrap();
    s["tasks"]["job"]["acceptance_round"] = json!(999);
    queue::save(&f.queue, &mut s).unwrap();
    assert!(delivery::prepare(&mut f.env, &f.queue, &f.spec, &delivery::local::Local).is_err());
    destroy_environment(&f.env).unwrap();
}
#[test]
fn frozen_parent_cannot_be_resumed_or_revised() {
    let mut f = Fixture::new(false);
    f.prepare();
    let mut s = queue::load_queue(&f.queue).unwrap();
    let v = s["tasks"]["job"]["state_version"].clone();
    assert!(progress::control(
        &mut s,
        Some(&json!("job")),
        Some(&v),
        Some(&json!("running")),
        Some(&json!("resume")),
        1.0
    )
    .is_err());
    assert!(lineage::action(&mut s,&json!({"kind":"candidate","task":"job","state_version":v,"candidate":"other","reason":"replace"}),1.0).is_err());
    delivery::execute(&mut f.env, &delivery::local::Local).unwrap();
    delivery::finish(&mut f.env).unwrap();
}
#[test]
fn destroys_live_task_processes_private_sessions_and_caches() {
    let mut f = Fixture::new(false);
    let root = herdr_flow::task_runtime::prepare(&f.env, "worker").unwrap();
    std::fs::write(root.join("data/session.json"), "private conversation").unwrap();
    std::fs::write(root.join("cache/large"), vec![42; 1024 * 1024]).unwrap();
    let mut child = f.env.command("/bin/sleep").arg("120").spawn().unwrap();
    f.env.register_process(child.id() as i32).unwrap();
    f.prepare();
    delivery::execute(&mut f.env, &delivery::local::Local).unwrap();
    delivery::finish(&mut f.env).unwrap();
    assert!(!process_record_is_alive(&ProcessRecord {
        pid: child.id() as i32,
        start_time: process_start_time(child.id() as i32)
    }));
    let _ = child.wait();
    assert!(!root.exists());
    f.clean();
}
#[test]
fn file_changes_and_target_aliases_cannot_replace_accepted_delivery() {
    let mut f = Fixture::new(false);
    f.prepare();
    f.env.mark_destroying().unwrap();
    assert!(delivery::execute(&mut f.env, &delivery::local::Local).is_err());
    assert!(!f.dir.path().join("customer").exists());
    f.env.mark_active().unwrap();
    let source = f.env.workspace_root.join("A");
    std::fs::write(&source, "changed").unwrap();
    assert!(delivery::execute(&mut f.env, &delivery::local::Local).is_err());
    assert!(!f.dir.path().join("customer").exists());
    std::fs::write(&source, "A").unwrap();
    std::os::unix::fs::symlink(&source, f.dir.path().join("customer")).unwrap();
    assert!(delivery::execute(&mut f.env, &delivery::local::Local).is_err());
    std::fs::remove_file(f.dir.path().join("customer")).unwrap();
    delivery::execute(&mut f.env, &delivery::local::Local).unwrap();
    delivery::finish(&mut f.env).unwrap();
    f.clean();
}
#[test]
fn preparation_recovers_journal_before_queue_freeze_without_submitting() {
    let mut f = Fixture::new(false);
    let before = queue::load_queue(&f.queue).unwrap();
    f.prepare();
    let id = f.env.delivery["id"].clone();
    let mut before = before;
    queue::save(&f.queue, &mut before).unwrap();
    f.env.release_ownership();
    f.env = restore_environment("job").unwrap();
    f.prepare();
    assert_eq!(f.env.delivery["id"], id);
    assert!(!f.dir.path().join("customer").exists());
    delivery::execute(&mut f.env, &delivery::local::Local).unwrap();
    delivery::finish(&mut f.env).unwrap();
    f.clean();
}
#[test]
fn existing_pr_batch_and_active_writer_prerequisites_cannot_be_skipped() {
    let mut f = Fixture::new(false);
    let original = queue::load_queue(&f.queue).unwrap();
    for (key, value) in [
        ("pr", json!({"phase":"checks_pending"})),
        ("batch_delivery", json!({"phase":"pending"})),
        ("writer", json!({"phase":"active"})),
    ] {
        let mut s = original.clone();
        s["tasks"]["job"][key] = value;
        queue::save(&f.queue, &mut s).unwrap();
        assert!(
            delivery::prepare(&mut f.env, &f.queue, &f.spec, &delivery::local::Local).is_err(),
            "{key}"
        );
    }
    destroy_environment(&f.env).unwrap();
}
#[test]
fn shared_receiver_pointer_clears_only_this_queue_and_owned_delivery() {
    let mut f = Fixture::new(false);
    let guard = f.dir.path().join("shared-guard");
    let mut s = queue::load_queue(&f.queue).unwrap();
    util::atomic_write_json(&guard.join("state.json"),&json!({"task":"coordinator","external_delivery":{"queue":f.queue,"delivery":"only"},"keep":"other tasks"})).unwrap();
    s["guard_dir"] = json!(guard);
    queue::save(&f.queue, &mut s).unwrap();
    f.prepare();
    delivery::execute(&mut f.env, &delivery::local::Local).unwrap();
    delivery::finish(&mut f.env).unwrap();
    let g = util::read_state_json(&guard, "state.json").unwrap();
    assert!(g["external_delivery"].is_null());
    assert_eq!(g["keep"], "other tasks");
    f.clean();
}
#[test]
fn delivery_cli_rejects_incomplete_and_unknown_options() {
    for args in [
        vec![],
        vec!["prepare"],
        vec!["execute", "--task"],
        vec!["finish", "--bad", "x"],
        vec!["finish", "--task", "x", "--task", "y"],
    ] {
        assert_eq!(
            delivery::cli::cli(&args.into_iter().map(str::to_string).collect::<Vec<_>>()),
            2
        );
    }
    assert_eq!(delivery::cli::cli(&["--help".into()]), 0);
}
#[test]
fn shared_lane_references_must_be_resolved_before_delivery() {
    let mut f = Fixture::new(false);
    let mut s = queue::load_queue(&f.queue).unwrap();
    s["tasks"]["other"]["lane"] =
        json!({"completed":[{"members":[{"task":"job","evidence":"temporary"}]}]});
    queue::save(&f.queue, &mut s).unwrap();
    assert!(delivery::prepare(&mut f.env, &f.queue, &f.spec, &delivery::local::Local).is_err());
    assert!(!f.dir.path().join("customer").exists());
    destroy_environment(&f.env).unwrap();
}
#[test]
fn finished_cleanup_is_not_replaced_when_only_a_plan_remains() {
    let mut f = Fixture::new(false);
    f.prepare();
    delivery::execute(&mut f.env, &delivery::local::Local).unwrap();
    let moved = f.dir.path().join("moved");
    std::fs::rename(&f.env.workspace_root, &moved).unwrap();
    assert!(delivery::finish(&mut f.env).is_err());
    f.env.release_ownership();
    std::fs::remove_file(environment_state_path("job").unwrap()).unwrap();
    assert!(create_environment(
        "job",
        &f.dir.path().join("other-work"),
        vec![],
        &f.dir.path().join("other-temp")
    )
    .is_err());
    std::fs::rename(&moved, &f.env.workspace_root).unwrap();
    assert_eq!(
        delivery::cli::cli(&["finish".into(), "--task".into(), "job".into()]),
        0
    );
    f.clean();
}

#[test]
fn cli_runs_persisted_delivery_retry_and_no_record_completion() {
    let f = Fixture::new(false);
    let spec = f.env.temp_root.join("spec.json");
    util::atomic_write_json(&spec, &f.spec).unwrap();
    f.env.release_ownership();
    let run = |args: &[&str]| {
        let output = Command::new(env!("CARGO_BIN_EXE_herdr-flow"))
            .arg("delivery")
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<J>(&output.stdout).unwrap()
    };
    let prepared = run(&[
        "prepare",
        "--task",
        "job",
        "--queue",
        f.queue.to_str().unwrap(),
        "--spec",
        spec.to_str().unwrap(),
    ]);
    assert_eq!(prepared["phase"], "prepared");
    std::fs::write(f.dir.path().join("customer"), "wrong").unwrap();
    assert_eq!(run(&["execute", "--task", "job"])["phase"], "failed");
    std::fs::remove_file(f.dir.path().join("customer")).unwrap();
    assert_eq!(run(&["reconcile", "--task", "job"])["phase"], "absent");
    assert_eq!(run(&["retry", "--task", "job"])["phase"], "confirmed");
    assert_eq!(run(&["reconcile", "--task", "job"])["phase"], "confirmed");
    assert_eq!(run(&["finish", "--task", "job"])["finished"], true);
    assert_eq!(
        run(&["finish", "--task", "job"])["delivery"],
        "not_attested"
    );
    f.clean();
}

// Protocol acceptance only: no native agent, Herdr session or live queue.
#[test]
fn cli_created_environment_finishes_confirmed_delivery_before_fixture_drop() {
    use std::os::unix::fs::PermissionsExt;
    let _lock = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()
        .unwrap();
    let root = dir.path();
    let control = root.join("control");
    let work = root.join("work");
    let temp = root.join("temp");
    let target = root.join("customer");
    let queue = temp.join("queue");
    let spec = root.join("create.json");
    util::atomic_write_json(
        &spec,
        &json!({"workspace_root":work,"temp_root":temp,
        "delivery_targets":[{"path":target,"target_type":"formal"}]}),
    )
    .unwrap();
    let sentinels = [
        "user-file",
        "shared-queue.json",
        "shared-session",
        "shared-cache",
    ];
    for name in sentinels {
        std::fs::write(root.join(name), format!("preserve {name}")).unwrap();
    }
    let run = |args: &[&str], expected: i32| {
        let out = Command::new(env!("CARGO_BIN_EXE_herdr-flow"))
            .args(args)
            .env("HERDR_TASK_ENV_ROOT", &control)
            .env_remove("HERDR_TASK_ENV_LIMIT")
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(expected), "{args:?}: {out:?}");
        if expected == 0 {
            serde_json::from_slice::<J>(&out.stdout).unwrap()
        } else {
            json!({"error":String::from_utf8_lossy(&out.stderr)})
        }
    };
    assert_eq!(
        run(
            &[
                "task-env",
                "create",
                "--task",
                "job",
                "--spec",
                spec.to_str().unwrap()
            ],
            0
        )["state"],
        "active"
    );
    assert!(control.join("envs/job.json").is_file());
    let source = work.join("A");
    std::fs::write(&source, "A").unwrap();
    // Reuse independent approval/authorization fixtures, not a fabricated live verdict.
    let mut state = accepted(json!({"adapter":"file-sha256",
        "id":util::pyjson::sha256_hex(b"A"),"location":source}));
    queue::save(&queue, &mut state).unwrap();
    util::atomic_write_json(
        &queue.join("results/review.json"),
        &json!({"task":"review"}),
    )
    .unwrap();
    let delivery_spec = temp.join("delivery.json");
    util::atomic_write_json(
        &delivery_spec,
        &json!({"adapter":"file","target":target,
        "authorization":"publish","state_version":state["tasks"]["job"]["state_version"]}),
    )
    .unwrap();
    assert_eq!(
        run(
            &[
                "delivery",
                "prepare",
                "--task",
                "job",
                "--queue",
                queue.to_str().unwrap(),
                "--spec",
                delivery_spec.to_str().unwrap()
            ],
            0
        )["phase"],
        "prepared"
    );
    let before = std::fs::read(queue.join("queue.json")).unwrap();
    let refused = run(&["delivery", "finish", "--task", "job"], 2);
    assert!(refused["error"]
        .as_str()
        .unwrap()
        .contains("delivery is not confirmed"));
    assert!(work.is_dir() && temp.is_dir());
    assert!(!target.exists());
    assert_eq!(std::fs::read(queue.join("queue.json")).unwrap(), before);
    assert_eq!(
        run(&["delivery", "execute", "--task", "job"], 0)["phase"],
        "confirmed"
    );
    assert_eq!(
        run(&["delivery", "reconcile", "--task", "job"], 0)["phase"],
        "confirmed"
    );
    assert_eq!(std::fs::read(&target).unwrap(), b"A");
    assert_eq!(
        run(&["delivery", "finish", "--task", "job"], 0)["finished"],
        true
    );
    // Assert before TempDir drops: automatic fixture teardown cannot prove cleanup.
    assert!(root.is_dir());
    for path in [&work, &temp, &queue] {
        assert!(!path.exists(), "retained {}", path.display());
    }
    for name in ["envs/job.json", "plans/job.json", "locks/job.lock"] {
        assert!(!control.join(name).exists(), "retained {name}");
    }
    let registry: J = util::read_json(&control.join("registry.json")).unwrap();
    assert_eq!(registry["entries"], json!({}));
    let status = run(&["task-env", "status", "--task", "job"], 0);
    assert_eq!(status["state"], "absent");
    assert_eq!(status["occupied_slots"], 0);
    assert_eq!(std::fs::read(&target).unwrap(), b"A");
    for name in sentinels {
        assert_eq!(
            std::fs::read_to_string(root.join(name)).unwrap(),
            format!("preserve {name}")
        );
    }
    assert_eq!(
        run(&["delivery", "finish", "--task", "job"], 0)["delivery"],
        "not_attested"
    );
}
#[test]
fn in_flight_delivery_process_prevents_absence_and_retry() {
    let mut f = Fixture::new(false);
    f.prepare();
    delivery::execute(&mut f.env, &Uncertain).unwrap();
    let mut child = f
        .env
        .command("/bin/sleep")
        .arg("120")
        .env(
            "HERDR_DELIVERY_OPERATION",
            f.env.delivery["id"].as_str().unwrap(),
        )
        .spawn()
        .unwrap();
    f.env.register_process(child.id() as i32).unwrap();
    assert_eq!(
        delivery::reconcile(&mut f.env, &delivery::local::Local).unwrap()["phase"],
        "unknown"
    );
    assert!(delivery::retry(&mut f.env, &delivery::local::Local).is_err());
    child.kill().unwrap();
    child.wait().unwrap();
    assert_eq!(
        delivery::reconcile(&mut f.env, &delivery::local::Local).unwrap()["phase"],
        "absent"
    );
    delivery::retry(&mut f.env, &delivery::local::Local).unwrap();
    delivery::finish(&mut f.env).unwrap();
    f.clean();
}
#[test]
fn target_branch_drift_is_unknown_and_cannot_be_replayed() {
    let mut f = Fixture::new(true);
    f.prepare();
    std::fs::write(f.env.workspace_root.join("A"), "unaccepted").unwrap();
    git_cmd(&f.env.workspace_root, &["commit", "-am", "unaccepted"]);
    git_cmd(
        &f.env.workspace_root,
        &[
            "push",
            f.dir.path().join("customer").to_str().unwrap(),
            "HEAD:main",
        ],
    );
    assert_eq!(
        delivery::execute(&mut f.env, &delivery::local::Local).unwrap()["phase"],
        "unknown"
    );
    assert!(delivery::finish(&mut f.env).is_err());
    assert!(delivery::retry(&mut f.env, &delivery::local::Local).is_err());
    // Explicit fixture repair of the customer's endpoint, then observe only.
    git_cmd(
        &f.dir.path().join("customer"),
        &[
            "update-ref",
            "refs/heads/main",
            f.env.delivery["artifact"]["id"].as_str().unwrap(),
        ],
    );
    assert_eq!(
        delivery::reconcile(&mut f.env, &delivery::local::Local).unwrap()["phase"],
        "confirmed"
    );
    delivery::finish(&mut f.env).unwrap();
    f.clean();
}
#[test]
fn shared_sidecar_symlink_cannot_delete_external_files() {
    let mut f = Fixture::new(false);
    f.prepare();
    delivery::execute(&mut f.env, &delivery::local::Local).unwrap();
    let protected = f.dir.path().join("protected");
    std::fs::create_dir(&protected).unwrap();
    std::fs::write(protected.join("review.json"), "preserve").unwrap();
    let saved = f.dir.path().join("saved-results");
    std::fs::rename(f.queue.join("results"), &saved).unwrap();
    std::os::unix::fs::symlink(&protected, f.queue.join("results")).unwrap();
    assert!(delivery::finish(&mut f.env).is_err());
    assert_eq!(
        std::fs::read_to_string(protected.join("review.json")).unwrap(),
        "preserve"
    );
    assert!(occupied_slots().unwrap() > 0);
    std::fs::remove_file(f.queue.join("results")).unwrap();
    std::fs::rename(&saved, f.queue.join("results")).unwrap();
    delivery::finish(&mut f.env).unwrap();
    f.clean();
    util::remove_state_file(&f.dir.path().join("absent"), "gone.json").unwrap();
    assert!(util::remove_state_file(f.dir.path(), "../escape").is_err());
}
#[test]
fn git_hooks_cannot_leak_task_files_outside_owned_roots_and_target() {
    use std::os::unix::fs::PermissionsExt;
    let mut f = Fixture::new(true);
    f.prepare();
    let leaked = f.dir.path().join("must-not-exist");
    let hook = f.env.workspace_root.join(".git/hooks/pre-push");
    std::fs::write(
        &hook,
        format!("#!/bin/sh\necho leaked > '{}'\n", leaked.display()),
    )
    .unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(
        delivery::execute(&mut f.env, &delivery::local::Local).unwrap()["phase"],
        "absent"
    );
    assert!(!leaked.exists());
    assert!(delivery::finish(&mut f.env).is_err());
    std::fs::remove_file(&hook).unwrap();
    assert_eq!(
        delivery::retry(&mut f.env, &delivery::local::Local).unwrap()["phase"],
        "confirmed"
    );
    delivery::finish(&mut f.env).unwrap();
    f.clean();
}

#[test]
fn malformed_removal_plan_preserves_shared_entries_and_recovers_without_redelivery() {
    let mut f = Fixture::new(false);
    f.prepare();
    delivery::execute(&mut f.env, &delivery::local::Local).unwrap();
    let before = queue::load_queue(&f.queue).unwrap();
    for plan in [
        json!({}),
        json!({"tasks":[], "waits":null, "events":[]}),
        json!({"tasks":[], "waits":false, "events":[]}),
        json!({"tasks":[], "waits":["review", null], "events":[]}),
        json!({"tasks":[], "waits":["review"], "events":false}),
    ] {
        f.env.delivery["removal"] = plan;
        f.env.persist().unwrap();
        let error = delivery::finish(&mut f.env).unwrap_err();
        assert!(error.message.contains("removal."), "{error}");
        assert!(f.queue.join("results/review.json").exists());
        assert_eq!(queue::load_queue(&f.queue).unwrap(), before);
        assert!(occupied_slots().unwrap() > 0);
        assert_eq!(f.env.delivery["phase"], "cleaning");
        assert!(delivery::execute(&mut f.env, &delivery::local::Local).is_err());
        f.env.release_ownership();
        let binary = std::env::var_os("HERDR_TEST_RELEASE_BIN")
            .unwrap_or_else(|| env!("CARGO_BIN_EXE_herdr-flow").into());
        let output = Command::new(binary)
            .args(["delivery", "finish", "--task", "job"])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2), "{output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("removal."),
            "{output:?}"
        );
        assert!(f.queue.join("results/review.json").exists());
        assert_eq!(queue::load_queue(&f.queue).unwrap(), before);
        f.env = restore_environment("job").unwrap();
    }
    f.env.delivery["removal"] = J::Null;
    f.env.persist().unwrap();
    f.env.release_ownership();
    f.env = restore_environment("job").unwrap();
    delivery::finish(&mut f.env).unwrap();
    f.clean();
}

// This is a protocol-fixture continuity check, not twenty real agent tasks.
// Reuse one control/queue directory so TempDir teardown cannot hide growth.
fn rearm_file_delivery(f: &mut Fixture, round: usize) {
    let target = f.dir.path().join(format!("customer-{round}"));
    f.env = create_environment(
        "job",
        &f.dir.path().join("work"),
        vec![DeliveryTarget::new(target.clone(), "formal".into())],
        &f.dir.path().join("tmp"),
    )
    .unwrap();
    let source = f.env.workspace_root.join("A");
    std::fs::write(&source, "A").unwrap();
    let fresh = accepted(
        json!({"adapter":"file-sha256","id":util::pyjson::sha256_hex(b"A"),"location":source}),
    );
    let mut state = queue::load_queue(&f.queue).unwrap();
    for collection in ["tasks", "waits", "events", "deliveries"] {
        for (key, value) in fresh[collection].as_object().unwrap() {
            state[collection][key] = value.clone();
        }
    }
    f.spec = json!({"adapter":"file","target":target,"authorization":"publish","state_version":state["tasks"]["job"]["state_version"]});
    queue::save(&f.queue, &mut state).unwrap();
}

fn retained_inventory(root: &Path, formal: &[PathBuf]) -> Vec<(PathBuf, u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let mut pending = vec![root.to_path_buf()];
    let mut paths = Vec::new();
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(directory).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if formal.contains(&path) {
                continue;
            }
            if entry.file_type().unwrap().is_dir() {
                pending.push(path.clone());
            }
            let relative = path.strip_prefix(root).unwrap().to_path_buf();
            let metadata = std::fs::symlink_metadata(&path).unwrap();
            // Queue contents are compared after removing only updated_at below.
            let bytes = if metadata.is_file() && relative != Path::new("shared-queue/queue.json") {
                metadata.len()
            } else {
                0
            };
            paths.push((relative, bytes, metadata.blocks()));
        }
    }
    paths.sort();
    paths
}

#[test]
fn twenty_protocol_deliveries_reuse_control_without_retained_task_state() {
    let mut f = Fixture::new(false);
    let mut formal = vec![f.dir.path().join("customer")];
    let user_asset = f.dir.path().join("user-asset");
    std::fs::write(&user_asset, "user-owned").unwrap();
    let mut baseline_paths = Vec::new();
    let mut baseline_queue = J::Null;
    for round in 0..20 {
        if round != 0 {
            rearm_file_delivery(&mut f, round);
            formal.push(f.dir.path().join(format!("customer-{round}")));
        }
        f.prepare();
        if round % 3 == 1 {
            f.env.release_ownership();
            f.env = restore_environment("job").unwrap();
        }
        delivery::execute(&mut f.env, &delivery::local::Local).unwrap();
        if round % 3 == 2 {
            f.env.release_ownership();
            f.env = restore_environment("job").unwrap();
            assert!(delivery::execute(&mut f.env, &delivery::local::Local).is_err());
        }
        delivery::finish(&mut f.env).unwrap();
        f.clean();
        for target in &formal {
            assert_eq!(std::fs::read_to_string(target).unwrap(), "A");
        }
        assert_eq!(std::fs::read_to_string(&user_asset).unwrap(), "user-owned");
        let paths = retained_inventory(f.dir.path(), &formal);
        println!(
            "round={round} formal={} retained_entries={} retained_blocks={}",
            formal.len(),
            paths.len(),
            paths.iter().map(|(_, _, blocks)| blocks).sum::<u64>()
        );
        let mut state = queue::load_queue(&f.queue).unwrap();
        state.as_object_mut().unwrap().remove("updated_at");
        if round == 0 {
            baseline_paths = paths;
            baseline_queue = state;
        } else {
            assert_eq!(paths, baseline_paths, "non-delivery paths grew at {round}");
            assert_eq!(state, baseline_queue, "shared state changed at {round}");
        }
    }
}
