//! Isolated authority and terminal barriers over disposable Git/queue fixtures.
use herdr_flow::{guard, lifecycle, model::J, progress, queue, task_env, util};
use serde_json::json;
use std::{
    fs,
    os::unix::{fs::PermissionsExt, process::CommandExt},
    path::{Path, PathBuf},
    process::Command,
    sync::{Mutex, MutexGuard},
};
static LOCK: Mutex<()> = Mutex::new(());
const SESSION: &str = "11111111-1111-1111-1111-111111111111";

struct Fixture {
    temp: tempfile::TempDir,
    queue: PathBuf,
    previous: Vec<(&'static str, Option<std::ffi::OsString>)>,
    _lock: MutexGuard<'static, ()>,
}
impl Fixture {
    fn new() -> Self {
        let lock = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::Builder::new()
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        let previous = [
            "HERDR_TASK_ENV_ROOT",
            "CODEX_THREAD_ID",
            task_env::MARKER_ENV,
            "HERDR_TASK_ID",
            "HERDR_TASK_QUEUE",
            "HERDR_TASK_EPOCH",
        ]
        .into_iter()
        .map(|key| (key, std::env::var_os(key)))
        .collect();
        std::env::set_var("HERDR_TASK_ENV_ROOT", temp.path().join("control"));
        std::env::set_var("CODEX_THREAD_ID", SESSION);
        let repo = temp.path().join("repo");
        private(&repo);
        git(&repo, &["init", "-q"]);
        git(
            &repo,
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.test",
                "commit",
                "--allow-empty",
                "-qm",
                "input",
            ],
        );
        let commit = git(&repo, &["rev-parse", "HEAD"]);
        let queue = temp.path().join("queue");
        queue::init_isolated(&queue, None, 30).unwrap();
        let receiver = temp.path().join("receiver");
        private(&receiver);
        util::protected::namespace(&receiver).unwrap();
        let state = json!({"version":2,"task":"coordinator","role":"coordinator","pane":"w1:p1","terminal":"term1",
            "socket":"/fixture","cwd":repo,"session":SESSION,"phase":"monitoring"});
        util::atomic_write_json(&receiver.join("state.json"), &state).unwrap();
        let mut s = queue::load_queue(&queue).unwrap();
        s["guard_dir"] = json!(receiver);
        s["receiver"] = guard::scope(&state);
        s["tasks"] = json!({"task":{"task":"task","epoch":1,"candidate":commit,"state_version":1,
            "phase":"waiting","control":"running","writer":null,"pending":[],"operations":{}}});
        s["waits"] = json!({"result":{"id":"result","task":"task","epoch":1,"kind":"signal","closed":false,
            "owner":"developer","reason":"fixture","condition":"done","timeout_seconds":30,
            "producer":{"adapter":"codex","session":SESSION}}});
        queue::save(&queue, &mut s).unwrap();
        Self {
            temp,
            queue,
            previous,
            _lock: lock,
        }
    }
    fn state(&self) -> J {
        queue::load_queue(&self.queue).unwrap()
    }
    fn call(&self, action: &str, spec: &J) -> herdr_flow::Result<J> {
        let _lock = queue::locked(&self.queue, false)?;
        let factory = |_: &J| -> herdr_flow::Result<Box<dyn herdr_flow::herdr::ReceiverAdapter>> {
            panic!("no Herdr or model calls in terminal fixture")
        };
        queue::task_action(&self.queue, action, spec, Some(SESSION), &factory)
    }
    fn create(&self) -> lifecycle::Owner {
        let s = self.state();
        self.call("task-environment",&json!({"action":"create","task":"task","queue_id":s["id"],"epoch":1,"state_version":1,
            "environment":{"workspace_root":self.temp.path().join("workspace"),"temp_root":self.temp.path().join("scratch"),
            "delivery_targets":[],"repository":self.temp.path().join("repo"),"commit":s["tasks"]["task"]["candidate"]}})).unwrap();
        lifecycle::task_owner(&self.state(), "task")
            .unwrap()
            .unwrap()
    }
    fn terminal_spec(&self, owner: &lifecycle::Owner, outcome: &str) -> J {
        json!({"task":"task","queue_id":owner.queue_id,"epoch":owner.epoch,"instance_id":owner.instance_id,
            "state_version":self.state()["tasks"]["task"]["state_version"],"outcome":outcome,"reason":"explicit final decision"})
    }
    fn save(&self, s: &mut J) -> herdr_flow::Result<()> {
        let _lock = queue::locked(&self.queue, false)?;
        queue::save(&self.queue, s)
    }
    fn marker(&self, o: &lifecycle::Owner) {
        for (key, value) in [
            (task_env::MARKER_ENV, o.instance_id.clone()),
            ("HERDR_TASK_ID", o.task_id.clone()),
            ("HERDR_TASK_QUEUE", o.queue_id.clone()),
            ("HERDR_TASK_EPOCH", o.epoch.to_string()),
        ] {
            std::env::set_var(key, value)
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        for (key, value) in &self.previous {
            match value {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
    }
}
fn private(path: &Path) {
    fs::create_dir(path).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap()
}
fn git(root: &Path, args: &[&str]) -> String {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(root).args(args);
    // SAFETY: scalar child-only creation mask; the test process mask is unchanged.
    unsafe {
        cmd.pre_exec(|| {
            libc::umask(0o077);
            Ok(())
        });
    }
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

#[test]
fn isolated_root_and_sidecars_reject_legacy_file_operations() {
    let f = Fixture::new();
    let owner = f.create();
    f.marker(&owner);
    queue::signal(&f.queue, "result", "failed", "intermediate failure", None).unwrap();
    let result = queue::artifacts(&f.queue)
        .unwrap()
        .join("results/result.json");
    for document in [f.queue.join("queue.json"), result] {
        assert!(document.is_dir());
        assert!(fs::read_to_string(&document).is_err());
        assert!(fs::remove_file(&document).is_err());
        let temporary = f.temp.path().join("legacy-snapshot.json");
        fs::write(&temporary, "{}").unwrap();
        assert!(fs::rename(&temporary, &document).is_err());
    }
    assert!(f.state()["tasks"]["task"]["final_outcome"].is_null());
    assert!(queue::init_isolated(&f.queue, None, 30).is_err());
    assert!(queue::init(&f.queue, None, 30).is_err());
}

#[test]
fn final_failure_atomically_records_obligation_and_keeps_live_resource_metadata() {
    let f = Fixture::new();
    let owner = f.create();
    let mut s = f.state();
    s["tasks"]["task"]["writer"] = json!({"session":"still-running"});
    s["tasks"]["task"]["operations"] = json!({"uncertain":{"phase":"sending"}});
    f.save(&mut s).unwrap();
    let spec = f.terminal_spec(&owner, "failure");
    let result = f.call("task-finalize", &spec).unwrap();
    let task = &result["tasks"]["task"];
    assert_eq!(task["final_outcome"], "failure");
    assert_eq!(task["phase"], "finalizing");
    assert_eq!(task["cleanup_obligation"]["owner"], json!(owner));
    assert_eq!(task["cleanup_obligation"]["status"], "pending");
    assert_eq!(task["writer"]["session"], "still-running");
    assert!(f.temp.path().join("workspace/checkout").is_dir());
    let repeated = f.call("task-finalize", &spec).unwrap();
    assert_eq!(repeated["tasks"]["task"], *task);
    assert!(f
        .call("task-finalize", &f.terminal_spec(&owner, "success"))
        .is_err());
}

#[test]
fn success_needs_confirmed_delivery_of_the_same_instance() {
    let f = Fixture::new();
    let owner = f.create();
    let spec = f.terminal_spec(&owner, "success");
    assert!(f.call("task-finalize", &spec).is_err());
    let mut env = task_env::restore_environment("task").unwrap();
    env.delivery = json!({"phase":"confirmed","instance":owner.instance_id,"task":"task"});
    env.persist().unwrap();
    drop(env);
    let s = f.call("task-finalize", &spec).unwrap();
    assert_eq!(s["tasks"]["task"]["final_outcome"], "success");
    assert_eq!(
        s["tasks"]["task"]["cleanup_obligation"]["status"],
        "pending"
    );
}

#[test]
fn shared_action_permit_blocks_terminal_and_terminal_blocks_future_writes() {
    let f = Fixture::new();
    let owner = f.create();
    let next = json!({"id":"next","task":"task","epoch":1,"kind":"signal","owner":"worker",
        "reason":"next bounded action","condition":"report","timeout_seconds":30});
    queue::add(&f.queue, &next).unwrap();
    let action = lifecycle::write_permit(&owner).unwrap();
    let spec = f.terminal_spec(&owner, "failure");
    assert!(f.call("task-finalize", &spec).is_err());
    assert!(f.state()["tasks"]["task"]["final_outcome"].is_null());
    drop(action);
    f.call("task-finalize", &spec).unwrap();
    assert!(lifecycle::write_permit(&owner).is_err());
    let mut late = next;
    late["id"] = json!("late");
    assert!(queue::add(&f.queue, &late).is_err());
    assert!(queue::signal(&f.queue, "result", "complete", "late", None).is_err());
    assert!(!queue::artifacts(&f.queue)
        .unwrap()
        .join("results/result.json")
        .exists());
}

#[test]
fn closing_refuses_takeover_apply_resume_and_ack_without_mutation() {
    let f = Fixture::new();
    let owner = f.create();
    let mut s = f.state();
    s["events"] = json!({"e":{"id":"e","task":"task","epoch":1,"acknowledged":false}});
    s["deliveries"] = json!({"d":{"id":"d","events":["e"]}});
    f.save(&mut s).unwrap();
    f.call("task-finalize", &f.terminal_spec(&owner, "failure"))
        .unwrap();
    let before = f.state();
    for action in ["task-begin", "task-accept", "task-apply", "task-resume"] {
        let spec = json!({"task":"task","state_version":before["tasks"]["task"]["state_version"],"operation":"op"});
        assert!(f.call(action, &spec).is_err(), "accepted {action}");
    }
    assert!(queue::acknowledge(&f.queue, "d", SESSION).is_err());
    assert_eq!(f.state(), before);
}

#[test]
fn observer_does_not_attach_or_revive_a_terminal_owner() {
    let f = Fixture::new();
    let owner = f.create();
    let factory = |_: &J| -> herdr_flow::Result<Box<dyn herdr_flow::herdr::ReceiverAdapter>> {
        panic!("notifications disabled")
    };
    queue::tick(&f.queue, &factory, None).unwrap();
    f.call("task-finalize", &f.terminal_spec(&owner, "failure"))
        .unwrap();
    let mut s = f.state();
    let before = s["tasks"]["task"].clone();
    queue::collect(&f.queue, &mut s, util::now()).unwrap();
    progress::stalls(&mut s, util::now() + 1000.0).unwrap();
    assert_eq!(s["tasks"]["task"], before);
    assert!(s["events"].as_object().unwrap().is_empty());
}

#[test]
fn stale_snapshot_epoch_and_instance_cannot_overwrite_new_authority() {
    let f = Fixture::new();
    let owner = f.create();
    let old = f.state();
    let mut erased = old.clone();
    erased.as_object_mut().unwrap().remove("lifecycle");
    assert!(f.save(&mut erased).is_err());
    assert_eq!(f.state(), old);
    for key in ["epoch", "instance_id"] {
        let mut s = old.clone();
        s["tasks"]["task"]["environment"][key] = json!("replacement");
        assert!(f.save(&mut s).is_err());
    }
    f.call("task-finalize", &f.terminal_spec(&owner, "failure"))
        .unwrap();
    let before = f.state();
    let mut stale = old;
    assert!(f.save(&mut stale).is_err());
    assert_eq!(f.state(), before);
}

#[test]
fn root_cancel_maps_failure_while_pause_and_child_completion_do_not() {
    let f = Fixture::new();
    let owner = f.create();
    let s = f.state();
    let paused=f.call("task-control",&json!({"task":"task","state_version":s["tasks"]["task"]["state_version"],"control":"paused","reason":"pause"})).unwrap();
    assert!(paused["tasks"]["task"]["final_outcome"].is_null());
    let cancelled=f.call("task-control",&json!({"task":"task","state_version":paused["tasks"]["task"]["state_version"],"control":"cancelled","reason":"do not continue"})).unwrap();
    assert_eq!(cancelled["tasks"]["task"]["final_outcome"], "failure");
    assert_eq!(
        cancelled["tasks"]["task"]["cleanup_obligation"]["owner"],
        json!(owner)
    );
}

#[test]
fn legacy_queue_remains_legacy_and_isolated_init_never_adopts_it() {
    let f = Fixture::new();
    let legacy = f.temp.path().join("legacy");
    let s = queue::init(&legacy, None, 30).unwrap();
    assert!(s["lifecycle"].is_null());
    assert!(legacy.join("queue.json").is_file());
    assert!(queue::init_isolated(&legacy, None, 30).is_err());
    assert_eq!(queue::artifacts(&legacy).unwrap(), legacy);
}

#[test]
fn old_command_path_is_rejected_before_reserving_or_spawning() {
    let f = Fixture::new();
    f.create();
    let sentinel = f.temp.path().join("executed");
    assert!(queue::run_command(
        &f.queue,
        "result",
        &json!(["touch", sentinel]),
        f.temp.path().to_str().unwrap()
    )
    .is_err());
    assert!(!sentinel.exists());
    assert!(!queue::artifacts(&f.queue)
        .unwrap()
        .join("commands")
        .exists());
}

#[test]
fn stale_producer_markers_and_saved_owner_never_become_current_results() {
    let f = Fixture::new();
    let owner = f.create();
    f.marker(&owner);
    std::env::set_var(task_env::MARKER_ENV, "old-instance");
    assert!(queue::signal(&f.queue, "result", "complete", "done", None).is_err());
    let path = queue::artifacts(&f.queue)
        .unwrap()
        .join("results/result.json");
    assert!(!path.exists());
    f.marker(&owner);
    queue::signal(&f.queue, "result", "complete", "done", None).unwrap();
    let mut result = util::read_json(&path).unwrap();
    assert_eq!(result["lifecycle_owner"], json!(owner));
    result["lifecycle_owner"]["instance_id"] = json!("old-instance");
    util::atomic_write_json(&path, &result).unwrap();
    assert!(queue::signal(&f.queue, "result", "complete", "done", None).is_err());
    let mut state = f.state();
    let before = state.clone();
    assert!(queue::collect(&f.queue, &mut state, util::now()).is_err());
    assert_eq!(state, before);
    let mut old = owner.clone();
    old.epoch += 1;
    assert!(lifecycle::write_permit(&old).is_err());
    old = owner;
    old.instance_id = "old-instance".into();
    assert!(lifecycle::write_permit(&old).is_err());
}

#[test]
fn v2_owner_requires_explicit_selected_git_artifact() {
    let f = Fixture::new();
    let mut s = f.state();
    let candidate = s["tasks"]["task"]["candidate"]
        .as_str()
        .unwrap()
        .to_string();
    s["tasks"]["task"]["schema_version"] = json!(2);
    s["tasks"]["task"]["requirements"] = json!({});
    s["tasks"]["task"]["artifacts"] = json!({});
    s["tasks"]["task"]["artifacts"][&candidate] = json!({"object":{"id":candidate,"adapter":"file-sha256","location":f.temp.path().join("repo")}});
    f.save(&mut s).unwrap();
    let spec = json!({"action":"create","task":"task","queue_id":s["id"],"epoch":1,"state_version":1,
        "environment":{"workspace_root":f.temp.path().join("workspace"),"temp_root":f.temp.path().join("scratch"),"delivery_targets":[],
            "repository":f.temp.path().join("repo"),"commit":candidate}});
    assert!(f.call("task-environment", &spec).is_err());
    assert!(!f.temp.path().join("workspace").exists());
    s["tasks"]["task"]["artifacts"][&candidate]["object"]["adapter"] = json!("git");
    f.save(&mut s).unwrap();
    let owner = f.create();
    assert_eq!(owner.task_id, "task");
}

#[test]
fn activity_uses_parent_owner_and_cannot_finalize_or_create_another_root() {
    let f = Fixture::new();
    let owner = f.create();
    let mut s = f.state();
    s["tasks"]["review"] = json!({"task":"review","epoch":9,"candidate":s["tasks"]["task"]["candidate"],"state_version":1,
        "phase":"waiting","control":"running","writer":null,"pending":[],"operations":{},"lineage":{"requirement":"task"}});
    f.save(&mut s).unwrap();
    assert_eq!(lifecycle::task_owner(&s, "review").unwrap().unwrap(), owner);
    let mut final_child = f.terminal_spec(&owner, "failure");
    final_child["task"] = json!("review");
    final_child["state_version"] = json!(1);
    assert!(f.call("task-finalize", &final_child).is_err());
    let create = json!({"action":"create","task":"review","queue_id":s["id"],"epoch":9,"state_version":1,
        "environment":{"workspace_root":f.temp.path().join("review"),"temp_root":f.temp.path().join("review-temp"),
            "delivery_targets":[],"repository":f.temp.path().join("repo"),"commit":s["tasks"]["review"]["candidate"]}});
    assert!(f.call("task-environment", &create).is_err());
    assert!(!f.temp.path().join("review").exists());
    s["tasks"]["review"]["phase"] = json!("completed");
    f.save(&mut s).unwrap();
    let factory = |_: &J| -> herdr_flow::Result<Box<dyn herdr_flow::herdr::ReceiverAdapter>> {
        panic!("notifications disabled")
    };
    queue::tick(&f.queue, &factory, None).unwrap();
    assert!(f.state()["tasks"]["task"]["final_outcome"].is_null());
}

#[test]
fn missing_queue_or_sidecar_markers_refuse_before_any_write_or_recreation() {
    let f = Fixture::new();
    let owner = f.create();
    f.marker(&owner);
    let state = f.state();
    let document = f.queue.join("queue.json");
    let payload = document.join("authority.json");
    let bytes = fs::read(&payload).unwrap();
    for (index, relative) in [
        ".flow-document-v2",
        ".flow-namespace-v2",
        "results/.flow-namespace-v2",
        "deliveries/.flow-namespace-v2",
    ]
    .into_iter()
    .enumerate()
    {
        let marker = document.join(relative);
        let held = f.temp.path().join(format!("held-{index}"));
        fs::rename(&marker, &held).unwrap();
        assert!(
            queue::artifacts(&f.queue).is_err(),
            "accepted missing {relative}"
        );
        assert!(queue::isolation::validate_layout(&f.queue, &state).is_err());
        assert!(queue::signal(&f.queue, "result", "complete", "late", None).is_err());
        let mut snapshot = state.clone();
        assert!(f.save(&mut snapshot).is_err());
        assert_eq!(snapshot, state);
        assert!(lifecycle::write_permit(&owner).is_err());
        assert!(!marker.exists());
        assert!(!document.join("results/result.json").exists());
        assert_eq!(fs::read(&payload).unwrap(), bytes);
        fs::rename(&held, &marker).unwrap();
    }
    assert_eq!(f.state(), state);
}
