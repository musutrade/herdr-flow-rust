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
