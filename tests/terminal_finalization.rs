//! No models or production resources: assertions precede fixture destruction.
use herdr_flow::{
    guard,
    lifecycle::{self, Owner},
    model::J,
    queue, task_env, util,
};
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
    old: Vec<(&'static str, Option<std::ffi::OsString>)>,
    _lock: MutexGuard<'static, ()>,
}

impl Fixture {
    fn new() -> Self {
        let lock = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::Builder::new()
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        let old = [
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
                "base",
            ],
        );
        let sha = git(&repo, &["rev-parse", "HEAD"]);
        let queue = temp.path().join("queue");
        queue::init_isolated(&queue, None, 30).unwrap();
        let receiver = temp.path().join("receiver");
        private(&receiver);
        util::protected::namespace(&receiver).unwrap();
        let guard = json!({"version":2,"task":"coordinator","role":"coordinator","pane":"w1:p1","terminal":"term1","socket":"/fixture","cwd":repo,"session":SESSION,"phase":"monitoring"});
        util::atomic_write_json(&receiver.join("state.json"), &guard).unwrap();
        let mut s = queue::load_queue(&queue).unwrap();
        s["guard_dir"] = json!(receiver);
        s["receiver"] = guard::scope(&guard);
        s["tasks"] = json!({"task":{"task":"task","epoch":1,"candidate":sha,"state_version":1,"phase":"waiting","control":"running","writer":null,"pending":[],"operations":{}}});
        queue::save(&queue, &mut s).unwrap();
        Self {
            temp,
            queue,
            old,
            _lock: lock,
        }
    }
    fn state(&self) -> J {
        queue::load_queue(&self.queue).unwrap()
    }
    fn save(&self, s: &mut J) {
        let _lock = queue::locked(&self.queue, false).unwrap();
        queue::save(&self.queue, s).unwrap();
    }
    fn create(&self) -> Owner {
        let s = self.state();
        self.call("task-environment", &json!({"action":"create","task":"task","queue_id":s["id"],"epoch":1,"state_version":1,"environment":{"workspace_root":self.temp.path().join("workspace"),"temp_root":self.temp.path().join("scratch"),"delivery_targets":[],"repository":self.temp.path().join("repo"),"commit":s["tasks"]["task"]["candidate"]}})).unwrap();
        lifecycle::task_owner(&self.state(), "task")
            .unwrap()
            .unwrap()
    }
    fn create_named(&self, name: &str) -> Owner {
        let mut s = self.state();
        let candidate = s["tasks"]["task"]["candidate"].clone();
        s["tasks"][name] = json!({"task":name,"epoch":1,"candidate":candidate,"state_version":1,
            "phase":"waiting","control":"running","writer":null,"pending":[],"operations":{}});
        self.save(&mut s);
        self.call("task-environment", &json!({"action":"create","task":name,"queue_id":s["id"],"epoch":1,"state_version":1,
            "environment":{"workspace_root":self.temp.path().join(format!("{name}-work")),"temp_root":self.temp.path().join(format!("{name}-tmp")),
            "delivery_targets":[],"repository":self.temp.path().join("repo"),"commit":candidate}})).unwrap();
        lifecycle::task_owner(&self.state(), name).unwrap().unwrap()
    }
    fn receiver(&self, owner: &Owner, session: &str) -> PathBuf {
        let env = task_env::restore_environment(&owner.task_id).unwrap();
        let root = herdr_flow::task_runtime::prepare(&env, "fixture-receiver").unwrap();
        assert_eq!(
            util::read_state_json(&root, "lifecycle-owner.json").unwrap(),
            json!(owner)
        );
        let g = json!({"version":2,"task":owner.task_id,"role":"coordinator","pane":"w1:p1","terminal":"term1",
            "socket":"/fixture","cwd":env.workspace_root.join("checkout"),"session":session,"phase":"monitoring"});
        util::atomic_write_json(&root.join("state.json"), &g).unwrap();
        root
    }
    fn call(&self, action: &str, spec: &J) -> herdr_flow::Result<J> {
        queue::finalization::task_action(&self.queue, action, spec, Some(SESSION), &|_| {
            panic!("no native or model calls")
        })
    }
    fn spec(&self, owner: &Owner, outcome: &str) -> J {
        json!({"task":"task","queue_id":owner.queue_id,"epoch":owner.epoch,"instance_id":owner.instance_id,"state_version":self.state()["tasks"]["task"]["state_version"],"outcome":outcome,"reason":"explicit final decision"})
    }
    fn record(&self, owner: &Owner) {
        let _lock = queue::locked(&self.queue, false).unwrap();
        queue::task_action(
            &self.queue,
            "task-finalize",
            &self.spec(owner, "failure"),
            Some(SESSION),
            &|_| panic!("no model calls"),
        )
        .unwrap();
    }
    fn result(&self, owner: &Owner) -> PathBuf {
        queue::add(&self.queue, &json!({"id":"r","task":"task","epoch":1,"kind":"signal","owner":"worker","reason":"fixture","condition":"reported","timeout_seconds":30})).unwrap();
        for (key, value) in [
            (task_env::MARKER_ENV, owner.instance_id.clone()),
            ("HERDR_TASK_ID", owner.task_id.clone()),
            ("HERDR_TASK_QUEUE", owner.queue_id.clone()),
            ("HERDR_TASK_EPOCH", owner.epoch.to_string()),
        ] {
            std::env::set_var(key, value);
        }
        queue::signal(&self.queue, "r", "failed", "intermediate result", None).unwrap();
        for key in [
            task_env::MARKER_ENV,
            "HERDR_TASK_ID",
            "HERDR_TASK_QUEUE",
            "HERDR_TASK_EPOCH",
        ] {
            std::env::remove_var(key);
        }
        queue::artifacts(&self.queue)
            .unwrap()
            .join("results/r.json")
    }
    fn freeze_before_resources(&self, owner: &Owner) -> J {
        let workspace = self.temp.path().join("workspace");
        let held = self.temp.path().join("held-workspace");
        fs::rename(&workspace, &held).unwrap();
        private(&workspace);
        let report = queue::finalization::drive(&self.queue).unwrap();
        assert_eq!(report.pending, 1, "{report:?}");
        let permit = lifecycle::acquire(owner, true).unwrap();
        let associations = task_env::terminal::association_plan(owner, &permit).unwrap();
        fs::remove_dir(&workspace).unwrap();
        fs::rename(&held, &workspace).unwrap();
        associations
    }
    fn clean(&self, owner: &Owner) {
        assert!(!self.temp.path().join("workspace").exists());
        assert!(!self.temp.path().join("scratch").exists());
        assert!(self.state()["tasks"]["task"].is_null());
        assert!(!task_env::is_registered("task").unwrap());
        assert!(!task_env::environment_state_path("task").unwrap().exists());
        assert!(!task_env::cleanup_state_path("task").unwrap().exists());
        assert!(task_env::terminal::pending_terminal_owners(&self.queue)
            .unwrap()
            .is_empty());
        assert!(!git(
            &self.temp.path().join("repo"),
            &["worktree", "list", "--porcelain"]
        )
        .contains("workspace/checkout"));
        assert!(lifecycle::write_permit(owner).is_err());
        assert!(!queue::artifacts(&self.queue)
            .unwrap()
            .join("gates")
            .join(format!("{}.lock", owner.task_id))
            .exists());
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        for (key, value) in &self.old {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}
fn private(path: &Path) {
    fs::create_dir(path).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}
fn git(root: &Path, args: &[&str]) -> String {
    let mut command = Command::new("git");
    command.arg("-C").arg(root).args(args);
    // SAFETY: scalar child-only umask; no process-global mutation.
    unsafe {
        command.pre_exec(|| {
            libc::umask(0o077);
            Ok(())
        });
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

#[test]
fn success_and_failure_automatically_remove_whole_dirty_roots_and_control_records() {
    for outcome in ["success", "failure"] {
        let f = Fixture::new();
        let owner = f.create();
        fs::write(
            f.temp.path().join("workspace/checkout/untracked"),
            "temporary",
        )
        .unwrap();
        fs::create_dir(f.temp.path().join("workspace/checkout/target")).unwrap();
        fs::write(
            f.temp.path().join("workspace/checkout/target/build"),
            "build",
        )
        .unwrap();
        fs::write(f.temp.path().join("scratch/log"), "temporary log").unwrap();
        let delivery = f.temp.path().join("formal-delivery");
        fs::write(&delivery, "keep").unwrap();
        if outcome == "success" {
            let mut env = task_env::restore_environment("task").unwrap();
            env.delivery = json!({"phase":"confirmed","task":"task","instance":owner.instance_id});
            env.persist().unwrap();
        }
        f.call("task-finalize", &f.spec(&owner, outcome)).unwrap();
        f.clean(&owner);
        assert_eq!(fs::read_to_string(delivery).unwrap(), "keep");
        let again = queue::finalization::drive(&f.queue).unwrap();
        assert_eq!(again.pending, 0);
        assert_eq!(again.completed, 0);
    }
}

#[test]
fn closing_checkpoint_recovers_even_when_queue_is_stopped_and_retains_foreign_task() {
    let f = Fixture::new();
    let owner = f.create();
    let mut s = f.state();
    s["tasks"]["review"] =
        json!({"task":"review","epoch":9,"lineage":{"requirement":"task"},"state_version":1});
    s["tasks"]["foreign"] =
        json!({"task":"foreign","epoch":3,"description":"task","state_version":1});
    s["waits"]["review-result"] = json!({"id":"review-result","task":"review","epoch":9});
    s["stopped"] = json!(true);
    f.save(&mut s);
    f.record(&owner);
    assert!(queue::validate_watch(&f.queue).is_ok());
    let report = queue::finalization::drive(&f.queue).unwrap();
    assert_eq!(report.pending, 0, "{report:?}");
    f.clean(&owner);
    let after = f.state();
    assert!(after["tasks"]["review"].is_null());
    assert!(after["waits"]["review-result"].is_null());
    assert_eq!(after["tasks"]["foreign"], s["tasks"]["foreign"]);
    assert!(queue::validate_watch(&f.queue).is_err());
}

#[test]
fn mixed_uncertain_delivery_retains_original_slot_and_can_ack_only_surviving_events() {
    for phase in ["sending", "uncertain", "submitted", "ack_overdue"] {
        let f = Fixture::new();
        let owner = f.create();
        let mut s = f.state();
        // An unbound foreign task cannot act on this delivery; bind it to the
        // same authority via an independent environment below.
        let mut foreign = s["tasks"]["task"].clone();
        foreign["task"] = json!("foreign");
        foreign.as_object_mut().unwrap().remove("environment");
        s["tasks"]["foreign"] = foreign;
        f.save(&mut s);
        f.call("task-environment", &json!({"action":"create","task":"foreign","queue_id":s["id"],"epoch":1,"state_version":s["tasks"]["foreign"]["state_version"],"environment":{"workspace_root":f.temp.path().join("foreign-work"),"temp_root":f.temp.path().join("foreign-tmp"),"delivery_targets":[],"repository":f.temp.path().join("repo"),"commit":s["tasks"]["task"]["candidate"]}})).unwrap();
        let mut s = f.state();
        s["waits"]["owned-wait"] = json!({"id":"owned-wait","task":"task","epoch":1});
        s["waits"]["foreign-wait"] = json!({"id":"foreign-wait","task":"foreign","epoch":1});
        s["events"] = json!({"owned":{"id":"owned","wait":"owned-wait","task":"task","epoch":1,"acknowledged":false},"survivor":{"id":"survivor","wait":"foreign-wait","task":"foreign","epoch":1,"acknowledged":false}});
        s["deliveries"] = json!({"mixed":{"id":"mixed","events":["owned","survivor"],"session":SESSION,"phase":phase,"sent_at":123.0}});
        s["active"] = json!("mixed");
        f.save(&mut s);
        let sidecar = queue::artifacts(&f.queue)
            .unwrap()
            .join("deliveries/mixed.json");
        util::atomic_write_json(&sidecar, &json!({"delivery":s["deliveries"]["mixed"],"events":[s["events"]["owned"],s["events"]["survivor"]]})).unwrap();
        let guard_dir = PathBuf::from(s["guard_dir"].as_str().unwrap());
        let mut g = util::read_state_json(&guard_dir, "state.json").unwrap();
        g["external_delivery"] = json!({"queue":f.queue,"delivery":"mixed","session":SESSION});
        guard::save(&guard_dir, &mut g).unwrap();
        f.call("task-finalize", &f.spec(&owner, "failure")).unwrap();
        f.clean(&owner);
        let after = f.state();
        assert_eq!(after["active"], "mixed");
        assert_eq!(after["deliveries"]["mixed"]["phase"], phase);
        assert_eq!(after["deliveries"]["mixed"]["events"], json!(["survivor"]));
        assert_eq!(
            util::read_json(&sidecar).unwrap()["events"],
            json!([s["events"]["survivor"]])
        );
        assert_eq!(
            util::read_state_json(&guard_dir, "state.json").unwrap()["external_delivery"],
            g["external_delivery"]
        );
        let _lock = queue::locked(&f.queue, false).unwrap();
        let acked = queue::acknowledge(&f.queue, "mixed", SESSION).unwrap();
        assert_eq!(acked["events"]["survivor"]["acknowledged"], true);
        assert!(acked["events"]["owned"].is_null());
        assert!(f.temp.path().join("foreign-work/checkout").is_dir());
    }
}

#[test]
fn all_owned_delivery_removes_sidecar_and_only_its_receiver_marker() {
    let f = Fixture::new();
    let owner = f.create();
    let mut s = f.state();
    s["waits"]["owned-wait"] = json!({"id":"owned-wait","task":"task","epoch":1});
    s["events"] = json!({"e":{"id":"e","wait":"owned-wait","task":"task","epoch":1}});
    s["deliveries"] = json!({"d":{"id":"d","events":["e"],"session":SESSION,"phase":"uncertain"}});
    s["active"] = json!("d");
    f.save(&mut s);
    let path = queue::artifacts(&f.queue)
        .unwrap()
        .join("deliveries/d.json");
    util::atomic_write_json(
        &path,
        &json!({"delivery":s["deliveries"]["d"],"events":[s["events"]["e"]]}),
    )
    .unwrap();
    let receiver = PathBuf::from(s["guard_dir"].as_str().unwrap());
    let mut g = util::read_state_json(&receiver, "state.json").unwrap();
    g["external_delivery"] = json!({"queue":f.queue,"delivery":"d","session":SESSION});
    guard::save(&receiver, &mut g).unwrap();
    f.call("task-finalize", &f.spec(&owner, "failure")).unwrap();
    f.clean(&owner);
    assert!(!path.exists());
    assert!(f.state()["active"].is_null());
    assert!(f.state()["deliveries"]["d"].is_null());
    assert!(util::read_state_json(&receiver, "state.json").unwrap()["external_delivery"].is_null());
}

#[test]
fn deleted_owner_cannot_be_resurrected_by_a_preterminal_snapshot() {
    let f = Fixture::new();
    let owner = f.create();
    let mut stale = f.state();
    f.call("task-finalize", &f.spec(&owner, "failure")).unwrap();
    f.clean(&owner);
    let before = f.state();
    let artifacts = queue::artifacts(&f.queue).unwrap();
    let _lock = queue::locked(&f.queue, false).unwrap();
    assert!(queue::save(&f.queue, &mut stale).is_err());
    assert_eq!(f.state(), before);
    assert!(!artifacts.join("results/late.json").exists());
    assert!(!f.temp.path().join("workspace").exists());
    assert!(lifecycle::acquire(&owner, true).is_err());
    assert!(!artifacts.join("gates/task.lock").exists());
}

#[test]
fn busy_cleanup_is_pending_and_next_system_attempt_reclaims_the_same_instance() {
    let f = Fixture::new();
    let owner = f.create();
    f.record(&owner);
    let before = f.state();
    let held = lifecycle::acquire(&owner, true).unwrap();
    let report = queue::finalization::drive(&f.queue).unwrap();
    assert_eq!(report.pending, 1);
    assert_eq!(report.completed, 0);
    assert_eq!(f.state(), before);
    assert!(f.temp.path().join("workspace/checkout").is_dir());
    drop(held);
    let report = queue::finalization::drive(&f.queue).unwrap();
    assert_eq!(report.pending, 0, "{report:?}");
    f.clean(&owner);
}

#[test]
fn replaced_root_refuses_cleanup_then_recovers_without_changing_final_outcome() {
    let f = Fixture::new();
    let owner = f.create();
    f.record(&owner);
    let workspace = f.temp.path().join("workspace");
    let held = f.temp.path().join("held-workspace");
    fs::rename(&workspace, &held).unwrap();
    private(&workspace);
    fs::write(workspace.join("foreign-sentinel"), "keep").unwrap();
    let report = queue::finalization::drive(&f.queue).unwrap();
    assert_eq!(report.pending, 1, "{report:?}");
    assert_eq!(f.state()["tasks"]["task"]["final_outcome"], "failure");
    assert_eq!(
        fs::read_to_string(workspace.join("foreign-sentinel")).unwrap(),
        "keep"
    );
    assert!(held.join("checkout").is_dir());
    assert!(task_env::is_registered("task").unwrap());
    // Restore only fixture paths; the product never adopts the replacement.
    let foreign = f.temp.path().join("foreign-replacement");
    fs::rename(&workspace, &foreign).unwrap();
    fs::rename(&held, &workspace).unwrap();
    let report = queue::finalization::drive(&f.queue).unwrap();
    assert_eq!(report.pending, 0, "{report:?}");
    f.clean(&owner);
    assert_eq!(
        fs::read_to_string(foreign.join("foreign-sentinel")).unwrap(),
        "keep"
    );
}

#[test]
fn frozen_sidecar_inode_proof_recovers_after_payload_and_marker_are_already_removed() {
    let f = Fixture::new();
    let owner = f.create();
    let result = f.result(&owner);
    f.record(&owner);
    let associations = f.freeze_before_resources(&owner);
    assert_eq!(associations["results"]["r"]["path"], json!(result));
    fs::remove_file(result.join("authority.json")).unwrap();
    fs::remove_file(result.join(".flow-document-v2")).unwrap();
    assert!(result.is_dir());
    let report = queue::finalization::drive(&f.queue).unwrap();
    assert_eq!(report.pending, 0, "{report:?}");
    f.clean(&owner);
    assert!(!result.exists());
    assert!(f.state()["waits"]["r"].is_null());
}

#[test]
fn replaced_sidecar_is_never_adopted_even_when_its_owner_payload_is_identical() {
    let f = Fixture::new();
    let owner = f.create();
    let result = f.result(&owner);
    let payload = util::read_json(&result).unwrap();
    f.record(&owner);
    f.freeze_before_resources(&owner);
    let held = f.temp.path().join("held-result");
    fs::rename(&result, &held).unwrap();
    util::atomic_write_json(&result, &payload).unwrap();
    let bytes = fs::read(result.join("authority.json")).unwrap();
    let report = queue::finalization::drive(&f.queue).unwrap();
    assert_eq!(report.pending, 1, "{report:?}");
    assert_eq!(fs::read(result.join("authority.json")).unwrap(), bytes);
    assert_eq!(f.state()["tasks"]["task"]["final_outcome"], "failure");
    let replacement = f.temp.path().join("replacement-result");
    fs::rename(&result, &replacement).unwrap();
    fs::rename(&held, &result).unwrap();
    let report = queue::finalization::drive(&f.queue).unwrap();
    assert_eq!(report.pending, 0, "{report:?}");
    f.clean(&owner);
    assert!(!result.exists());
    assert_eq!(fs::read(replacement.join("authority.json")).unwrap(), bytes);
}

#[test]
fn isolated_claim_admission_rejects_unbound_and_cross_owner_authority_without_writes() {
    for bound in [false, true] {
        let f = Fixture::new();
        if bound {
            f.create();
        }
        let before = f.state();
        for claim in [
            json!({"version":1,"kind":"member","root":"task","root_epoch":1,"admission_evidence":"fixture"}),
            json!({"version":1,"kind":"root","lane":"lane","admission_evidence":"fixture","previous":{"task":"task","epoch":1,"candidate":before["tasks"]["task"]["candidate"],"receipt_id":"receipt"}}),
        ] {
            let error = f
                .call("task-add", &json!({"task":"next","claim":claim}))
                .unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("same-root claims are unsupported"),
                "{error}"
            );
            assert_eq!(f.state(), before);
            assert!(
                herdr_flow::progress::claim::isolated_admission(&before, "next", &claim).is_err()
            );
            let mut legacy = before.clone();
            legacy.as_object_mut().unwrap().remove("lifecycle");
            assert!(
                herdr_flow::progress::claim::isolated_admission(&legacy, "next", &claim).is_ok()
            );
        }
        assert!(!f.temp.path().join("next").exists());
        if !bound {
            assert!(!f.temp.path().join("workspace").exists());
        }
    }
}

#[test]
fn existing_foreign_claim_chain_blocks_cleanup_without_erasing_authority_or_resources() {
    for claim in [
        json!({"kind":"member","root":"task","root_epoch":1}),
        json!({"kind":"root","previous":{"task":"task","epoch":1,"receipt_id":"receipt"}}),
    ] {
        let f = Fixture::new();
        let owner = f.create();
        let mut s = f.state();
        s["tasks"]["foreign"] = json!({"task":"foreign","epoch":2,"claim":claim});
        f.save(&mut s);
        f.record(&owner);
        let before = f.state();
        let report = queue::finalization::drive(&f.queue).unwrap();
        assert_eq!(report.pending, 1, "{report:?}");
        assert!(report.errors[0].contains("foreign live claim authority"));
        assert_eq!(f.state(), before);
        assert!(f.temp.path().join("workspace/checkout").is_dir());
        assert!(task_env::is_registered("task").unwrap());
        assert!(!task_env::cleanup_state_path("task").unwrap().exists());
    }
}

#[test]
fn typed_shared_git_receipts_remove_exact_members_without_inserting_unrelated_fields() {
    let f = Fixture::new();
    let owner = f.create();
    let mut s = f.state();
    let sha = s["tasks"]["task"]["candidate"].clone();
    s["tasks"]["task"]["batch_delivery"] =
        json!({"owner":"lane","phase":"accepted","head":sha,"receipt_id":"receipt"});
    s["tasks"]["review"] = json!({"task":"review","epoch":9,"candidate":sha,"state_version":1,"lineage":{"requirement":"task"},"batch_delivery":{"owner":"lane","phase":"accepted","head":sha,"receipt_id":"review-receipt"}});
    let owned = json!({"task":"task","epoch":1,"head":sha,"receipt_id":"receipt","checkout":f.temp.path().join("workspace/checkout")});
    let activity = json!({"task":"review","epoch":9,"head":sha,"receipt_id":"review-receipt"});
    let sibling =
        json!({"task":"sibling","epoch":7,"head":"other","receipt_id":"foreign","note":"task"});
    s["tasks"]["lane"] = json!({"task":"lane","epoch":3,"state_version":4,"description":"task","lane":{"pending":[owned,sibling],"active":{"id":"batch","phase":"accepted","members":[activity,sibling]},"history":[{"id":"history","members":[owned,sibling]},{"id":"no-members","description":"task"}]}});
    s["tasks"]["minimal-lane"] =
        json!({"task":"minimal-lane","epoch":5,"lane":{"description":"task"}});
    s["tasks"]["no-lane"] = json!({"task":"no-lane","epoch":8,"description":"review"});
    f.save(&mut s);
    let mut expected = s["tasks"]["lane"].clone();
    expected["lane"]["pending"] = json!([sibling]);
    expected["lane"]["active"]["members"] = json!([sibling]);
    expected["lane"]["history"][0]["members"] = json!([sibling]);
    expected["state_version"] = json!(5);
    f.call("task-finalize", &f.spec(&owner, "failure")).unwrap();
    f.clean(&owner);
    let after = f.state();
    assert_eq!(after["tasks"]["lane"], expected);
    assert_eq!(after["tasks"]["minimal-lane"], s["tasks"]["minimal-lane"]);
    assert_eq!(after["tasks"]["no-lane"], s["tasks"]["no-lane"]);
    assert!(after["tasks"]["review"].is_null());
}

#[test]
fn shared_receipt_tuple_or_live_claim_mismatch_refuses_before_resource_removal() {
    for fault in [
        "epoch",
        "receipt",
        "lane",
        "phase",
        "source-claim",
        "receiver-claim",
    ] {
        let f = Fixture::new();
        let owner = f.create();
        let mut s = f.state();
        let sha = s["tasks"]["task"]["candidate"].clone();
        s["tasks"]["task"]["batch_delivery"] =
            json!({"owner":"lane","phase":"accepted","head":sha,"receipt_id":"receipt"});
        let mut member = json!({"task":"task","epoch":1,"head":sha,"receipt_id":"receipt"});
        match fault {
            "epoch" => member["epoch"] = json!(2),
            "receipt" => member["receipt_id"] = json!("different"),
            "lane" => s["tasks"]["task"]["batch_delivery"]["owner"] = json!("different"),
            "phase" => s["tasks"]["task"]["batch_delivery"]["phase"] = json!("pending"),
            "source-claim" => {
                s["tasks"]["task"]["batch_delivery"]["claimed_by"] =
                    json!({"task":"lane","epoch":3})
            }
            "receiver-claim" => (),
            _ => unreachable!(),
        }
        s["tasks"]["lane"] =
            json!({"task":"lane","epoch":3,"state_version":1,"lane":{"pending":[member]}});
        if fault == "receiver-claim" {
            s["tasks"]["lane"]["batch_delivery"] = json!({"claimed_by":{"task":"task","epoch":1}});
        }
        f.save(&mut s);
        f.record(&owner);
        let before = f.state();
        let report = queue::finalization::drive(&f.queue).unwrap();
        assert_eq!(report.pending, 1, "{fault}: {report:?}");
        assert_eq!(f.state(), before, "{fault}");
        assert!(f.temp.path().join("workspace/checkout").is_dir(), "{fault}");
        assert!(task_env::is_registered("task").unwrap());
    }
}

#[test]
fn old_epoch_foreign_wait_and_mismatched_assignment_cannot_authorize_association_removal() {
    for fault in [
        "wait-epoch",
        "event-epoch",
        "event-wait",
        "event-assignment",
        "result-wait",
        "result-task",
        "result-epoch",
        "result-queue",
    ] {
        let f = Fixture::new();
        let owner = f.create();
        let result_path = f.result(&owner);
        let mut s = f.state();
        s["waits"]["r"]["assignment"] = json!("assignment");
        s["waits"]["r"]["candidate"] = s["tasks"]["task"]["candidate"].clone();
        let binding = herdr_flow::progress::binding(&s["waits"]["r"]).unwrap();
        s["events"]["e"] = json!({"id":"e","task":"task","epoch":1,"wait":"r","binding":binding,"lifecycle_owner":owner});
        s["tasks"]["foreign"] = json!({"task":"foreign","epoch":1});
        s["waits"]["foreign-wait"] = json!({"id":"foreign-wait","task":"foreign","epoch":1});
        let mut result = util::read_json(&result_path).unwrap();
        match fault {
            "wait-epoch" => s["waits"]["r"]["epoch"] = json!(2),
            "event-epoch" => s["events"]["e"]["epoch"] = json!(2),
            "event-wait" => s["events"]["e"]["wait"] = json!("foreign-wait"),
            "event-assignment" => s["events"]["e"]["binding"]["assignment"] = json!("foreign"),
            "result-wait" => result["wait"] = json!("foreign-wait"),
            "result-task" => result["task"] = json!("foreign"),
            "result-epoch" => result["epoch"] = json!(2),
            "result-queue" => result["queue"] = json!("foreign"),
            _ => unreachable!(),
        }
        f.save(&mut s);
        util::atomic_write_json(&result_path, &result).unwrap();
        f.record(&owner);
        let before = f.state();
        let bytes = fs::read(result_path.join("authority.json")).unwrap();
        let report = queue::finalization::drive(&f.queue).unwrap();
        assert_eq!(report.pending, 1, "{fault}: {report:?}");
        assert_eq!(f.state(), before, "{fault}");
        assert_eq!(fs::read(result_path.join("authority.json")).unwrap(), bytes);
        assert!(f.temp.path().join("workspace/checkout").is_dir(), "{fault}");
    }
}

#[test]
fn same_name_receiver_requires_owned_runtime_and_registered_root_proof() {
    for has_marker in [false, true] {
        let f = Fixture::new();
        let owner = f.create();
        let mut s = f.state();
        let root = PathBuf::from(s["guard_dir"].as_str().unwrap());
        let mut g = util::read_state_json(&root, "state.json").unwrap();
        g["task"] = json!("task");
        s["receiver"] = guard::scope(&g);
        guard::save(&root, &mut g).unwrap();
        if has_marker {
            util::protected::runtime_namespace(&root).unwrap();
            util::atomic_write_json(&root.join("lifecycle-owner.json"), &json!(owner)).unwrap();
        }
        f.save(&mut s);
        f.record(&owner);
        let before = f.state();
        let guard_bytes = fs::read(root.join("state.json/authority.json")).unwrap();
        let report = queue::finalization::drive(&f.queue).unwrap();
        assert_eq!(report.pending, 1, "{report:?}");
        assert_eq!(f.state(), before);
        assert_eq!(
            fs::read(root.join("state.json/authority.json")).unwrap(),
            guard_bytes
        );
        assert!(f.temp.path().join("workspace/checkout").is_dir());
    }
}

#[test]
fn proven_owned_receiver_is_removed_with_its_root_and_shared_receiver_is_untouched() {
    let f = Fixture::new();
    let owner = f.create();
    let mut s = f.state();
    let shared = PathBuf::from(s["guard_dir"].as_str().unwrap());
    let shared_bytes = fs::read(shared.join("state.json/authority.json")).unwrap();
    let root = f.temp.path().join("workspace/receiver");
    private(&root);
    util::protected::runtime_namespace(&root).unwrap();
    util::atomic_write_json(&root.join("lifecycle-owner.json"), &json!(owner)).unwrap();
    let mut g = util::read_state_json(&shared, "state.json").unwrap();
    g["task"] = json!("task");
    g["cwd"] = json!(f.temp.path().join("workspace/checkout"));
    util::atomic_write_json(&root.join("state.json"), &g).unwrap();
    s["guard_dir"] = json!(root);
    s["receiver"] = guard::scope(&g);
    s["notify"] = json!(true);
    f.save(&mut s);
    f.call("task-finalize", &f.spec(&owner, "failure")).unwrap();
    f.clean(&owner);
    assert!(!root.exists());
    assert!(f.state()["receiver"].is_null());
    assert!(f.state()["guard_dir"].is_null());
    assert_eq!(f.state()["notify"], false);
    assert_eq!(
        fs::read(shared.join("state.json/authority.json")).unwrap(),
        shared_bytes
    );
}

#[test]
fn stall_cleanup_requires_the_recorded_wait_and_each_current_binding_field() {
    for fault in ["none", "wait", "task", "epoch", "assignment", "candidate"] {
        let f = Fixture::new();
        let owner = f.create();
        let mut s = f.state();
        s["tasks"]["task"]["wait"] = json!("r");
        s["tasks"]["task"]["assignment"] = json!("assignment");
        s["waits"]["r"] = json!({"id":"r","task":"task","epoch":1,"assignment":"assignment","candidate":s["tasks"]["task"]["candidate"]});
        let binding = herdr_flow::progress::binding(&s["waits"]["r"]).unwrap();
        s["events"]["stall"] = json!({"id":"stall","task":"task","epoch":1,"wait":"r","kind":"progress_stall","binding":binding,"acknowledged":false});
        match fault {
            "none" => (),
            "wait" => s["events"]["stall"]["wait"] = json!("foreign-wait"),
            "epoch" => s["events"]["stall"]["binding"]["epoch"] = json!(2),
            key => s["events"]["stall"]["binding"][key] = json!("foreign"),
        }
        f.save(&mut s);
        f.record(&owner);
        let before = f.state();
        let report = queue::finalization::drive(&f.queue).unwrap();
        if fault == "none" {
            assert_eq!(report.completed, 1, "{report:?}");
            f.clean(&owner);
            assert!(f.state()["events"]["stall"].is_null());
        } else {
            assert_eq!(report.pending, 1, "{fault}: {report:?}");
            assert_eq!(f.state(), before);
            assert!(f.temp.path().join("workspace/checkout").is_dir());
        }
    }
}

#[test]
fn active_delivery_observation_preserves_submission_and_freezes_before_closing_owner_effects() {
    let f = Fixture::new();
    let owner = f.create();
    let mut s = f.state();
    s["waits"]["r"] = json!({"id":"r","task":"task","epoch":1});
    s["events"]["e"] = json!({"id":"e","task":"task","epoch":1,"wait":"r","acknowledged":false});
    s["deliveries"]["d"] = json!({"id":"d","events":["e"],"session":SESSION,"phase":"submitted","sent_at":util::now()});
    s["active"] = json!("d");
    f.save(&mut s);
    let submitted = s["deliveries"]["d"].clone();
    let factory = |_: &J| -> herdr_flow::Result<Box<dyn herdr_flow::herdr::ReceiverAdapter>> {
        panic!("existing submission cannot be resent")
    };
    {
        let _lock = queue::locked(&f.queue, false).unwrap();
        queue::dispatch(&f.queue, &mut s, &factory).unwrap();
    }
    assert_eq!(f.state()["deliveries"]["d"], submitted);
    assert_eq!(f.state()["active"], "d");
    assert_eq!(f.state()["events"]["e"]["acknowledged"], false);
    f.record(&owner);
    let before = f.state();
    let mut closing = before.clone();
    {
        let _lock = queue::locked(&f.queue, false).unwrap();
        queue::dispatch(&f.queue, &mut closing, &factory).unwrap();
    }
    assert_eq!(closing, before);
    assert_eq!(f.state(), before);
    let report = queue::finalization::drive(&f.queue).unwrap();
    assert_eq!(report.completed, 1, "{report:?}");
    f.clean(&owner);
}

fn retiring_receiver_fixture() -> (Fixture, Owner, Owner, Owner, PathBuf) {
    retiring_receiver_fixture_at_checkpoint(false)
}

fn frozen_shared_receiver_fixture() -> (Fixture, Owner, Owner, Owner, PathBuf) {
    let f = Fixture::new();
    let a = f.create();
    let b = f.create_named("receiver-owner");
    let c = f.create_named("survivor");
    let receiver = f.receiver(&b, SESSION);
    let mut g = util::read_state_json(&receiver, "state.json").unwrap();
    let mut s = f.state();
    s["guard_dir"] = json!(receiver);
    s["receiver"] = guard::scope(&g);
    for (id, owner) in [("a", &a), ("b", &b), ("c", &c)] {
        let wait = format!("wait-{id}");
        s["waits"][&wait] = json!({"id":wait,"task":owner.task_id,"epoch":owner.epoch});
        s["events"][id] = json!({"id":id,"task":owner.task_id,"epoch":owner.epoch,"wait":wait,"acknowledged":false});
    }
    s["deliveries"]["mixed"] =
        json!({"id":"mixed","events":["a","b","c"],"session":SESSION,"phase":"uncertain"});
    s["active"] = json!("mixed");
    f.save(&mut s);
    util::atomic_write_json(&queue::artifacts(&f.queue).unwrap().join("deliveries/mixed.json"),
        &json!({"delivery":s["deliveries"]["mixed"],"events":[s["events"]["a"],s["events"]["b"],s["events"]["c"]]})).unwrap();
    g["external_delivery"] = json!({"queue":f.queue,"delivery":"mixed","session":SESSION});
    guard::save(&receiver, &mut g).unwrap();
    f.record(&a);
    let associations = f.freeze_before_resources(&a);
    assert_eq!(associations["receiver_owner"], json!(b));
    (f, a, b, c, receiver)
}

fn retiring_receiver_fixture_at_checkpoint(
    omit_dependency: bool,
) -> (Fixture, Owner, Owner, Owner, PathBuf) {
    let (f, a, b, c, receiver) = frozen_shared_receiver_fixture();
    let frozen_path = task_env::cleanup_state_path(&a.task_id)
        .unwrap()
        .join("authority.json");
    let frozen_bytes = fs::read(&frozen_path).unwrap();
    if omit_dependency {
        // Reconstruct root-prepare committed / queue dependency not committed.
        // Ordinary save must never authorize removing terminal metadata.
        let _queue = queue::locked(&f.queue, false).unwrap();
        let mut checkpoint = f.state();
        checkpoint["tasks"][&a.task_id]["cleanup_obligation"]
            .as_object_mut()
            .unwrap()
            .remove("receiver_dependency");
        assert!(queue::save(&f.queue, &mut checkpoint).is_err());
        util::atomic_write_json(&f.queue.join("queue.json"), &checkpoint).unwrap();
    }
    let held_a = if omit_dependency {
        Some(lifecycle::acquire(&a, true).unwrap())
    } else {
        None
    };
    let version = f.state()["tasks"][&b.task_id]["state_version"].clone();
    f.call(
        "task-finalize",
        &json!({"task":b.task_id,"queue_id":b.queue_id,"epoch":b.epoch,"instance_id":b.instance_id,
        "state_version":version,"outcome":"failure","reason":"receiver owner explicitly retired"}),
    )
    .unwrap();
    assert!(f.state()["tasks"][&b.task_id].is_null());
    assert!(!receiver.exists());
    assert!(!task_env::is_registered(&b.task_id).unwrap());
    assert!(!queue::artifacts(&f.queue)
        .unwrap()
        .join("gates")
        .join(format!("{}.lock", b.task_id))
        .exists());
    assert_eq!(
        f.state()["tasks"][&a.task_id]["cleanup_obligation"]["receiver_dependency"]["retired_by"],
        json!(b)
    );
    if omit_dependency {
        assert_eq!(fs::read(&frozen_path).unwrap(), frozen_bytes);
    }
    drop(held_a);
    (f, a, b, c, receiver)
}

#[test]
fn retired_receiver_owner_does_not_strand_another_terminal_mixed_delivery() {
    let (f, a, _b, c, receiver) = retiring_receiver_fixture();
    let survivor = f.state()["tasks"][&c.task_id].clone();
    let report = queue::finalization::drive(&f.queue).unwrap();
    assert_eq!(report.completed, 1, "{report:?}");
    assert_eq!(report.pending, 0, "{report:?}");
    f.clean(&a);
    assert!(!receiver.exists());
    assert_eq!(f.state()["tasks"][&c.task_id], survivor);
    assert!(f.temp.path().join("survivor-work/checkout").is_dir());
    assert_eq!(
        f.state()["deliveries"]["mixed"],
        json!({"id":"mixed","events":["c"],"session":SESSION,"phase":"uncertain"})
    );
    assert_eq!(f.state()["events"]["c"]["acknowledged"], false);
    assert_eq!(
        util::read_json(
            &queue::artifacts(&f.queue)
                .unwrap()
                .join("deliveries/mixed.json")
        )
        .unwrap()["events"],
        json!([f.state()["events"]["c"]])
    );
}

#[test]
fn retired_receiver_cleanup_never_touches_a_replacement_guard_or_session() {
    let (f, a, _b, c, old_receiver) = retiring_receiver_fixture();
    let replacement = f.receiver(&c, "22222222-2222-2222-2222-222222222222");
    let mut g = util::read_state_json(&replacement, "state.json").unwrap();
    g["external_delivery"] =
        json!({"queue":f.queue,"delivery":"another-delivery","session":g["session"]});
    guard::save(&replacement, &mut g).unwrap();
    let mut s = f.state();
    s["guard_dir"] = json!(replacement);
    s["receiver"] = guard::scope(&g);
    f.save(&mut s);
    let bytes = fs::read(replacement.join("state.json/authority.json")).unwrap();
    let scope = s["receiver"].clone();
    let report = queue::finalization::drive(&f.queue).unwrap();
    assert_eq!(report.completed, 1, "{report:?}");
    f.clean(&a);
    assert!(!old_receiver.exists());
    assert_eq!(
        fs::read(replacement.join("state.json/authority.json")).unwrap(),
        bytes
    );
    assert_eq!(f.state()["receiver"], scope);
    assert_eq!(f.state()["guard_dir"], json!(replacement));
    assert_eq!(f.state()["deliveries"]["mixed"]["phase"], "uncertain");
    assert_eq!(f.state()["deliveries"]["mixed"]["events"], json!(["c"]));
    assert_eq!(f.state()["events"]["c"]["acknowledged"], false);
}

#[test]
fn retired_receiver_recovers_root_prepare_before_dependency_queue_checkpoint() {
    let (f, a, b, c, receiver) = retiring_receiver_fixture_at_checkpoint(true);
    let before = f.state();
    let proof = &before["tasks"][&a.task_id]["cleanup_obligation"]["receiver_dependency"];
    assert_eq!(proof["owner"], json!(b));
    assert_eq!(proof["retired_by"], json!(b));
    let report = queue::finalization::drive(&f.queue).unwrap();
    assert_eq!(report.completed, 1, "{report:?}");
    f.clean(&a);
    assert!(!receiver.exists());
    assert_eq!(f.state()["tasks"][&c.task_id], before["tasks"][&c.task_id]);
    assert_eq!(f.state()["deliveries"]["mixed"]["events"], json!(["c"]));
    assert_eq!(f.state()["events"]["c"]["acknowledged"], false);
    assert_eq!(queue::finalization::drive(&f.queue).unwrap().completed, 0);
}

#[test]
fn retired_receiver_proof_cannot_be_rebound_to_another_owner_scope_or_path() {
    for field in ["owner", "guard_dir", "scope", "retired_by"] {
        let (f, a, _b, c, receiver) = retiring_receiver_fixture_at_checkpoint(true);
        let mut s = f.state();
        let dependency = &mut s["tasks"][&a.task_id]["cleanup_obligation"]["receiver_dependency"];
        match field {
            "owner" | "retired_by" => dependency[field] = json!(c),
            "guard_dir" => dependency[field] = json!(f.temp.path().join("foreign-receiver")),
            "scope" => dependency[field]["session"] = json!("foreign-session"),
            _ => unreachable!(),
        }
        {
            let _queue = queue::locked(&f.queue, false).unwrap();
            let original = f.state();
            assert!(queue::save(&f.queue, &mut s).is_err());
            assert_eq!(f.state(), original);
            // Only the adversarial fixture can bypass the ordinary fence.
            util::atomic_write_json(&f.queue.join("queue.json"), &s).unwrap();
        }
        let before = f.state();
        let report = queue::finalization::drive(&f.queue).unwrap();
        assert_eq!(report.pending, 1, "{field}: {report:?}");
        assert_eq!(f.state(), before);
        assert!(!receiver.exists());
        assert!(f.temp.path().join("workspace/checkout").is_dir());
        assert!(f.temp.path().join("survivor-work/checkout").is_dir());
    }
}

#[test]
fn existing_guard_lock_never_recreates_a_disappeared_receiver() {
    let f = Fixture::new();
    let path = f.temp.path().join("lock-receiver");
    private(&path);
    let lock = guard::locked_existing(&path).unwrap();
    assert!(guard::locked_existing(&path).is_err());
    drop(lock);
    fs::remove_file(path.join("guard.lock")).unwrap();
    fs::remove_dir(&path).unwrap();
    assert!(guard::locked_existing(&path).is_err());
    assert!(!path.exists());
    // The ordinary bootstrap API retains its original directory creation.
    drop(guard::locked(&path).unwrap());
    assert!(path.join("guard.lock").is_file());
}

#[test]
fn shared_receiver_without_retirement_rejects_missing_or_same_scope_foreign_owner() {
    for replace in [false, true] {
        let (f, a, b, c, receiver) = frozen_shared_receiver_fixture();
        let held = f.temp.path().join("held-receiver");
        fs::rename(&receiver, &held).unwrap();
        let mut replacement_bytes = None;
        if replace {
            let replacement = f.receiver(&c, SESSION);
            fs::rename(&replacement, &receiver).unwrap();
            // Identical scope cannot substitute for the frozen complete Owner.
            let old_guard = util::read_state_json(&held, "state.json").unwrap();
            util::atomic_write_json(&receiver.join("state.json"), &old_guard).unwrap();
            replacement_bytes = Some(fs::read(receiver.join("state.json/authority.json")).unwrap());
        }
        let before = f.state();
        let report = queue::finalization::drive(&f.queue).unwrap();
        assert_eq!(report.pending, 1, "replace={replace}: {report:?}");
        assert!(
            report
                .errors
                .iter()
                .any(|e| e.contains("cleanup receiver owner changed")),
            "replace={replace}: receiver identity must be the actual refusal: {report:?}"
        );
        assert_eq!(f.state(), before);
        assert!(task_env::is_registered(&b.task_id).unwrap());
        if let Some(bytes) = replacement_bytes {
            assert_eq!(
                fs::read(receiver.join("state.json/authority.json")).unwrap(),
                bytes
            );
            assert_eq!(
                util::read_state_json(&receiver, "lifecycle-owner.json").unwrap(),
                json!(c)
            );
            fs::rename(&receiver, f.temp.path().join("foreign-receiver-preserved")).unwrap();
        } else {
            assert!(!receiver.exists());
        }
        fs::rename(&held, &receiver).unwrap();
        assert_eq!(
            util::read_state_json(&receiver, "lifecycle-owner.json").unwrap(),
            json!(b)
        );
        assert_eq!(
            guard::scope(&util::read_state_json(&receiver, "state.json").unwrap()),
            f.state()["receiver"]
        );
        drop(guard::locked_existing(&receiver).unwrap());
        let report = queue::finalization::drive(&f.queue).unwrap();
        let plan = util::read_json(&task_env::cleanup_state_path(&a.task_id).unwrap()).ok();
        assert_eq!(
            report.completed,
            1,
            "replace={replace}: {report:?}; retained terminal phase={:?} last_error={:?}",
            plan.as_ref().map(|p| &p["terminal"]["phase"]),
            plan.as_ref().map(|p| &p["last_error"])
        );
        f.clean(&a);
        assert!(receiver.exists());
        assert_eq!(
            f.state()["deliveries"]["mixed"]["events"],
            json!(["b", "c"])
        );
        assert_eq!(f.state()["events"]["b"]["acknowledged"], false);
    }
}

#[test]
fn mixed_sidecar_filter_before_queue_commit_recovers_without_ack_or_resend() {
    let f = Fixture::new();
    let owner = f.create();
    let survivor = f.create_named("survivor");
    let mut s = f.state();
    for (id, task) in [("owned", &owner), ("survivor", &survivor)] {
        s["waits"][id] = json!({"id":id,"task":task.task_id,"epoch":task.epoch});
        s["events"][id] =
            json!({"id":id,"wait":id,"task":task.task_id,"epoch":task.epoch,"acknowledged":false});
    }
    s["deliveries"]["mixed"] =
        json!({"id":"mixed","events":["owned","survivor"],"session":SESSION,"phase":"uncertain"});
    s["active"] = json!("mixed");
    f.save(&mut s);
    let path = queue::artifacts(&f.queue)
        .unwrap()
        .join("deliveries/mixed.json");
    util::atomic_write_json(&path, &json!({"delivery":s["deliveries"]["mixed"],"events":[s["events"]["owned"],s["events"]["survivor"]]})).unwrap();
    let receiver = PathBuf::from(s["guard_dir"].as_str().unwrap());
    let mut g = util::read_state_json(&receiver, "state.json").unwrap();
    g["external_delivery"] = json!({"queue":f.queue,"delivery":"mixed","session":SESSION});
    guard::save(&receiver, &mut g).unwrap();
    let guard_before = fs::read(receiver.join("state.json/authority.json")).unwrap();
    f.record(&owner);
    let before = f.state();
    let lock = guard::locked_existing(&receiver).unwrap();
    let report = queue::finalization::drive(&f.queue).unwrap();
    assert_eq!(report.pending, 1, "{report:?}");
    assert!(
        report
            .errors
            .iter()
            .any(|error| error.contains("guard state is busy")),
        "{report:?}"
    );
    assert_eq!(f.state(), before);
    assert!(!f.temp.path().join("workspace").exists());
    assert!(!f.temp.path().join("scratch").exists());
    assert_eq!(
        util::read_json(&path).unwrap()["events"],
        json!([s["events"]["survivor"]])
    );
    assert_eq!(
        fs::read(receiver.join("state.json/authority.json")).unwrap(),
        guard_before
    );
    drop(lock);
    let report = queue::finalization::drive(&f.queue).unwrap();
    assert_eq!(report.completed, 1, "{report:?}");
    f.clean(&owner);
    assert_eq!(
        f.state()["deliveries"]["mixed"],
        json!({"id":"mixed","events":["survivor"],"session":SESSION,"phase":"uncertain"})
    );
    assert_eq!(f.state()["events"]["survivor"]["acknowledged"], false);
    assert_eq!(
        f.state()["tasks"][&survivor.task_id],
        before["tasks"][&survivor.task_id]
    );
    assert_eq!(
        fs::read(receiver.join("state.json/authority.json")).unwrap(),
        guard_before
    );
    let after = f.state();
    assert_eq!(queue::finalization::drive(&f.queue).unwrap().completed, 0);
    assert_eq!(f.state(), after);
}

#[test]
fn late_old_tuple_cannot_touch_new_epoch_instance_at_the_same_task_and_paths() {
    let f = Fixture::new();
    let old = f.create();
    let old_result_path = f.result(&old);
    let old_result = util::read_json(&old_result_path).unwrap();
    let mut snapshot = f.state();
    let mut replacement = snapshot["tasks"]["task"].clone();
    f.call("task-finalize", &f.spec(&old, "failure")).unwrap();
    f.clean(&old);
    replacement.as_object_mut().unwrap().remove("environment");
    replacement["epoch"] = json!(2);
    replacement["state_version"] = json!(1);
    let mut s = f.state();
    s["tasks"]["task"] = replacement;
    f.save(&mut s);
    f.call("task-environment", &json!({"action":"create","task":"task","queue_id":s["id"],"epoch":2,"state_version":1,
        "environment":{"workspace_root":f.temp.path().join("workspace"),"temp_root":f.temp.path().join("scratch"),
        "delivery_targets":[],"repository":f.temp.path().join("repo"),"commit":s["tasks"]["task"]["candidate"]}})).unwrap();
    let current = lifecycle::task_owner(&f.state(), "task").unwrap().unwrap();
    assert_eq!(current.epoch, 2);
    assert_ne!(current.instance_id, old.instance_id);
    fs::write(
        f.temp.path().join("workspace/new-instance-sentinel"),
        "keep",
    )
    .unwrap();
    queue::add(&f.queue, &json!({"id":"r","task":"task","epoch":2,"kind":"signal","owner":"worker","reason":"fixture","condition":"reported","timeout_seconds":30})).unwrap();
    let before = f.state();
    let state_path = task_env::environment_state_path("task")
        .unwrap()
        .join("authority.json");
    let document = fs::read(&state_path).unwrap();
    {
        let _queue = queue::locked(&f.queue, false).unwrap();
        assert!(queue::save(&f.queue, &mut snapshot).is_err());
    }
    assert!(f.call("task-finalize", &f.spec(&old, "failure")).is_err());
    let stale = lifecycle::acquire(&old, true).unwrap();
    assert!(stale.check(&before, true).is_err());
    assert!(task_env::terminal::resume_terminal(&old, &stale).is_err());
    drop(stale);
    for (key, value) in [
        (task_env::MARKER_ENV, old.instance_id.clone()),
        ("HERDR_TASK_ID", old.task_id.clone()),
        ("HERDR_TASK_QUEUE", old.queue_id.clone()),
        ("HERDR_TASK_EPOCH", old.epoch.to_string()),
    ] {
        std::env::set_var(key, value);
    }
    assert!(queue::signal(&f.queue, "r", "failed", "late old producer", None).is_err());
    for key in [
        task_env::MARKER_ENV,
        "HERDR_TASK_ID",
        "HERDR_TASK_QUEUE",
        "HERDR_TASK_EPOCH",
    ] {
        std::env::remove_var(key);
    }
    assert!(!old_result_path.exists());
    util::atomic_write_json(&old_result_path, &old_result).unwrap();
    let mut observed = f.state();
    {
        let _queue = queue::locked(&f.queue, false).unwrap();
        assert!(queue::collect(&f.queue, &mut observed, util::now()).is_err());
    }
    assert_eq!(observed, before);
    assert_eq!(f.state(), before);
    assert_eq!(fs::read(&state_path).unwrap(), document);
    assert_eq!(
        fs::read_to_string(f.temp.path().join("workspace/new-instance-sentinel")).unwrap(),
        "keep"
    );
    assert!(lifecycle::write_permit(&current).is_ok());
    util::protected::remove_document(&old_result_path).unwrap();
    assert_eq!(queue::finalization::drive(&f.queue).unwrap().completed, 0);
    assert_eq!(f.state(), before);
    f.call("task-finalize", &f.spec(&current, "failure"))
        .unwrap();
    f.clean(&current);
}

fn await_fixture_file(path: &Path) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !path.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "fixture handshake missing: {}",
            path.display()
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

struct DriverChild(std::process::Child);
impl DriverChild {
    fn spawn(queue: &Path, barrier: &Path, id: &str) -> Self {
        Self(
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "competing_finalization_driver_worker",
                    "--ignored",
                    "--nocapture",
                ])
                .env("FLOW_FIXTURE_QUEUE", queue)
                .env("FLOW_FIXTURE_BARRIER", barrier)
                .env("FLOW_FIXTURE_DRIVER", id)
                .stdin(std::process::Stdio::null())
                .spawn()
                .unwrap(),
        )
    }
    fn finish(&mut self) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            if let Some(status) = self.0.try_wait().unwrap() {
                assert!(status.success(), "driver worker failed: {status}");
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "driver worker timed out"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}
impl Drop for DriverChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
#[ignore = "fixture subprocess, invoked by competing_real_drivers_finish_one_obligation_without_recreating_authority"]
fn competing_finalization_driver_worker() {
    let queue = PathBuf::from(std::env::var_os("FLOW_FIXTURE_QUEUE").unwrap());
    let barrier = PathBuf::from(std::env::var_os("FLOW_FIXTURE_BARRIER").unwrap());
    let id = std::env::var("FLOW_FIXTURE_DRIVER").unwrap();
    fs::write(barrier.join(format!("ready-{id}")), "ready").unwrap();
    await_fixture_file(&barrier.join("blocked-go"));
    let blocked = queue::finalization::drive(&queue).unwrap();
    assert_eq!(blocked.pending, 1, "{blocked:?}");
    assert!(
        blocked.errors.iter().any(|error| error.contains("busy")),
        "{blocked:?}"
    );
    fs::write(barrier.join(format!("blocked-{id}")), "blocked").unwrap();
    await_fixture_file(&barrier.join("go"));
    let result = queue::finalization::drive(&queue).unwrap();
    util::atomic_write_json(&barrier.join(format!("result-{id}.json")), &json!(result)).unwrap();
}

#[test]
fn competing_real_drivers_finish_one_obligation_without_recreating_authority() {
    let f = Fixture::new();
    let owner = f.create();
    fs::write(f.temp.path().join("workspace/checkout/dirty"), "temporary").unwrap();
    f.record(&owner);
    let held = lifecycle::acquire(&owner, true).unwrap();
    let barrier = f.temp.path().join("driver-barrier");
    private(&barrier);
    let mut a = DriverChild::spawn(&f.queue, &barrier, "a");
    let mut b = DriverChild::spawn(&f.queue, &barrier, "b");
    for id in ["a", "b"] {
        await_fixture_file(&barrier.join(format!("ready-{id}")));
    }
    fs::write(barrier.join("blocked-go"), "go").unwrap();
    for id in ["a", "b"] {
        await_fixture_file(&barrier.join(format!("blocked-{id}")));
    }
    assert!(f.temp.path().join("workspace/checkout/dirty").exists());
    drop(held);
    fs::write(barrier.join("go"), "go").unwrap();
    a.finish();
    b.finish();
    let results =
        ["a", "b"].map(|id| util::read_json(&barrier.join(format!("result-{id}.json"))).unwrap());
    let completed: u64 = results
        .iter()
        .map(|result| result["completed"].as_u64().unwrap())
        .sum();
    assert_eq!(completed, 1, "{results:?}");
    let report = queue::finalization::drive(&f.queue).unwrap();
    assert_eq!(report.pending, 0, "{report:?}");
    f.clean(&owner);
    let after = f.state();
    assert_eq!(queue::finalization::drive(&f.queue).unwrap().completed, 0);
    assert_eq!(f.state(), after);
}
