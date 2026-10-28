use super::*;
use crate::task_env::{self, review::ReviewWorkspace, RootIdentity};
use std::fs;
use std::os::unix::fs::MetadataExt;

const ASSIGNMENT: &str = "accept-original";
const WAIT: &str = "original-result";
const CHILD: &str = "acceptance-activity";
const SESSION: &str = "11111111-1111-1111-1111-111111111111";

struct Fixture {
    _temp: tempfile::TempDir,
    _root: task_env::TestEnvRootGuard,
    env: TaskEnvironment,
    state: J,
    owner: Owner,
    runtime: PathBuf,
    coordinator: PathBuf,
    binding: J,
    collected: J,
}

impl Fixture {
    fn new() -> Self {
        let temp = util::fs::test_tempdir();
        let root = task_env::test_env_root(&temp.path().join("control"));
        let mut env = task_env::create_environment(
            "original-parent",
            &temp.path().join("workspace"),
            vec![],
            &temp.path().join("scratch"),
        )
        .unwrap();
        let owner = Owner {
            queue_root: temp.path().join("queue"),
            queue_id: "original-queue".into(),
            task_id: env.task_id.clone(),
            epoch: 1,
            instance_id: env.instance_id.clone(),
        };
        // Selection is read-only. Seed the exact ownership tuple without
        // launching a model, broker, Git materialization, or native frontend.
        env.creation = Some(serde_json::from_value(json!({
            "binding":{"queue_id":owner.queue_id,"task_id":owner.task_id,"epoch":1},
            "lifecycle_queue":owner.queue_root,"phase":"ready",
            "repository":env.workspace_root,"repository_identity":env.workspace_identity,
            "common_dir":env.workspace_root,"common_identity":env.workspace_identity,
            "commit":"fixture","checkout":env.workspace_root,
            "checkout_identity":env.workspace_identity,"git_identity":null,"git_admin_before":null
        })).unwrap());
        env.state = task_env::EnvironmentState::Active;
        env.delivery = json!({"id":"stage-original","stage":{"epoch":4}});
        env.child_processes.push(task_env::RegisteredProcess {
            pid: 424242,
            pgid: 424242,
            start_time: Some(42),
            descendants: vec![],
            direct_children: vec![],
        });
        let runtime = env.temp_root.join("rt").join(ASSIGNMENT);
        let coordinator = env.temp_root.join("rt/coordinator");
        for directory in [&runtime, &coordinator, &temp.path().join("guard")] {
            util::ensure_private_dir(directory, true).unwrap();
        }
        let binding = json!({"task":CHILD,"epoch":2,"assignment":ASSIGNMENT,
            "candidate":"original-artifact","artifact_adapter":"local_git",
            "lineage":{"requirement":owner.task_id,"scope_epoch":4,"role":"acceptance",
                "revision":1,"session_epoch":2,"input":"original-artifact"}});
        let runtime_record = json!({"adapter":"codex","configuration":"upstream",
            "root":runtime,"instance":env.instance_id,"lifecycle_owner":owner,
            "owner":task_env::instance::owner(&env),"pid":424242,"start_time":42,
            "upstream_socket":{"path":temp.path().join("shared-server.sock"),"dev":1,"ino":2,"uid":0}});
        write(&runtime, "runtime.json", &runtime_record);
        write(
            &runtime,
            "creation.json",
            &json!({"phase":"bound","packet":{"binding":binding},"session":SESSION}),
        );
        write(
            &runtime,
            "turn.json",
            &json!({"phase":"signalled","binding":binding,"session":SESSION,"outcome":"complete"}),
        );
        write(
            &runtime,
            "upstream-session.json",
            &json!({"phase":"created","instance":env.instance_id,
            "session":SESSION,"runtime":runtime_record}),
        );
        write(
            &temp.path().join("guard"),
            "state.json",
            &json!({"runtime":coordinator}),
        );
        let mut wait = binding.clone();
        wait["runtime"] = json!(runtime);
        let state = json!({"guard_dir":temp.path().join("guard"),"waits":{WAIT:wait},
            "tasks":{CHILD:binding},"events":{WAIT:{"wait":WAIT,"binding":binding,"acknowledged":true,"applied":true}}});
        let collected = json!({"collection":{"phase":"signalled","binding":binding},
            "result":{"contentItems":[{"type":"text","text":json!({"runtime":runtime,"wait":WAIT}).to_string()}]}});
        collection(&coordinator, &owner, &collected).unwrap();
        Self {
            _temp: temp,
            _root: root,
            env,
            state,
            owner,
            runtime,
            coordinator,
            binding,
            collected,
        }
    }

    fn plan(&self) -> Plan {
        selection::prepare(
            &self.env,
            &self.state,
            self.owner.clone(),
            "stage-original".into(),
        )
        .unwrap()
    }

    fn retained(&self, expected_reason: &str) {
        let plan = self.plan();
        assert!(
            plan.entries.is_empty(),
            "unexpected selected entry: {expected_reason}"
        );
        assert_eq!(plan.retained.len(), 1);
        let reason = plan.retained[0]["reason"].as_str().unwrap();
        assert!(
            reason.contains(expected_reason),
            "expected {expected_reason:?}, got {reason:?}"
        );
        assert!(self.runtime.join("runtime.json").is_file());
        assert!(!self.runtime.join(MARKER).exists());
    }

    fn edit(&self, name: &str, edit: impl FnOnce(&mut J)) {
        let mut value = read(&self.runtime, name).unwrap();
        edit(&mut value);
        write(&self.runtime, name, &value);
    }

    fn review(&mut self) -> PathBuf {
        let path = self.env.workspace_root.join("reviews").join(ASSIGNMENT);
        util::ensure_private_dir(&path.join(".git"), true).unwrap();
        fs::write(path.join("result"), "review evidence").unwrap();
        self.env.review_workspaces.insert(
            ASSIGNMENT.into(),
            ReviewWorkspace {
                owner: self.owner.clone(),
                binding: self.binding.clone(),
                wait: WAIT.into(),
                runtime: self.runtime.clone(),
                path: path.clone(),
                source: self.env.workspace_root.join("checkout"),
                phase: task_env::review::Phase::Ready,
                identity: Some(RootIdentity::of(&fs::metadata(&path).unwrap())),
                git_identity: Some(RootIdentity::of(&fs::metadata(path.join(".git")).unwrap())),
            },
        );
        path
    }
}

fn write(root: &Path, name: &str, value: &J) {
    util::atomic_write_json(&root.join(name), value).unwrap();
}

#[test]
fn selects_only_the_original_bound_acked_applied_result_without_mutation() {
    let f = Fixture::new();
    let before = fs::read(f.runtime.join("runtime.json")).unwrap();
    let plan = f.plan();
    assert_eq!(plan.entries.len(), 1);
    assert!(plan.retained.is_empty());
    assert_eq!(plan.owner, f.owner);
    assert_eq!(plan.scope_epoch, 4);
    assert_eq!(plan.entries[0].root, f.runtime);
    assert_eq!(plan.entries[0].wait, WAIT);
    assert_eq!(plan.entries[0].binding, f.binding);
    assert_eq!(
        plan.entries[0].broker,
        ProcessRecord {
            pid: 424242,
            start_time: Some(42)
        }
    );
    assert_eq!(plan.entries[0].sessions[0]["session"], SESSION);
    assert!(plan.entries[0].presentation.is_none());
    assert_eq!(fs::read(f.runtime.join("runtime.json")).unwrap(), before);
    assert!(!f.runtime.join(MARKER).exists());
    assert!(f.env.workspace_root.is_dir());
}

#[test]
fn wrong_parent_scope_and_non_worker_roles_are_not_adopted() {
    for (field, value) in [
        ("scope_epoch", json!(5)),
        ("requirement", json!("another-parent")),
        ("role", json!("publication")),
    ] {
        let mut f = Fixture::new();
        f.state["waits"][WAIT]["lineage"][field] = value;
        let plan = f.plan();
        assert!(plan.entries.is_empty());
        assert!(plan.retained.is_empty());
        assert!(f.runtime.join("creation.json").is_file());
    }
}

#[test]
fn unknown_collection_unacked_unapplied_and_changed_bindings_are_retained() {
    for fault in [
        "missing-receipt",
        "unacked",
        "unapplied",
        "event-binding",
        "activity-binding",
        "receipt-binding",
        "coordinator",
    ] {
        let mut f = Fixture::new();
        let expected = match fault {
            "missing-receipt" => {
                fs::remove_file(f.runtime.join(RECEIPT)).unwrap();
                "state document is missing"
            }
            "unacked" => {
                f.state["events"][WAIT]["acknowledged"] = json!(false);
                "ACKed/applied"
            }
            "unapplied" => {
                f.state["events"][WAIT]["applied"] = json!(false);
                "ACKed/applied"
            }
            "event-binding" => {
                f.state["events"][WAIT]["binding"]["candidate"] = json!("later");
                "ACKed/applied"
            }
            "activity-binding" => {
                f.state["tasks"][CHILD]["epoch"] = json!(3);
                "activity binding changed"
            }
            "receipt-binding" => {
                f.edit(RECEIPT, |v| v["binding"]["candidate"] = json!("later"));
                "persisted collection"
            }
            "coordinator" => {
                f.edit(RECEIPT, |v| {
                    v["coordinator"] = json!("/another/coordinator")
                });
                "coordinator changed"
            }
            _ => unreachable!(),
        };
        f.retained(expected);
    }
}

#[test]
fn unknown_or_inprogress_worker_creation_turn_and_session_are_retained() {
    for (name, field, value, reason) in [
        (
            "creation.json",
            "phase",
            json!("created"),
            "creation is unknown",
        ),
        (
            "creation.json",
            "phase",
            json!("creating"),
            "creation is unknown",
        ),
        (
            "creation.json",
            "session",
            json!("22222222-2222-2222-2222-222222222222"),
            "creation is unknown",
        ),
        (
            "turn.json",
            "phase",
            json!("in_progress"),
            "turn has not settled",
        ),
        ("turn.json", "outcome", J::Null, "turn has not settled"),
        (
            "upstream-session.json",
            "phase",
            json!("unknown"),
            "session ownership is unknown",
        ),
        (
            "upstream-session.json",
            "instance",
            json!("foreign-instance"),
            "session ownership is unknown",
        ),
    ] {
        let f = Fixture::new();
        f.edit(name, |journal| journal[field] = value);
        f.retained(reason);
    }
    let f = Fixture::new();
    f.edit("creation.json", |journal| {
        journal["packet"]["binding"]["epoch"] = json!(99)
    });
    f.retained("creation is unknown");
    drop(f);
    let f = Fixture::new();
    f.edit("upstream-session.json", |journal| {
        journal["runtime"]["upstream_socket"]["ino"] = json!(999)
    });
    f.retained("session ownership is unknown");
}

#[test]
fn recovery_publication_coordinator_and_unresolved_operations_keep_evidence() {
    for fault in [
        "recovery",
        "publication",
        "receiver",
        "pending-native",
        "pending-control",
        "incident",
        "descendants",
        "broker-identity",
    ] {
        let mut f = Fixture::new();
        let reason = match fault {
            "recovery" => {
                f.edit("runtime.json", |v| v["recovery_role"] = json!(true));
                "recovery and publication"
            }
            "publication" => {
                f.edit("runtime.json", |v| v["publication_role"] = json!(true));
                "recovery and publication"
            }
            "receiver" => {
                write(&f.runtime, "receiver.json", &json!({}));
                "coordinator resources"
            }
            "pending-native" => {
                write(
                    &f.runtime,
                    "broker-events.json",
                    &json!({"pending":{"request":{}}}),
                );
                "unresolved native"
            }
            "pending-control" => {
                write(
                    &f.runtime,
                    "control-tools.json",
                    &json!({"request":{"phase":"requested"}}),
                );
                "unresolved control"
            }
            "incident" => {
                let recovery = f.coordinator.join("recovery-observer");
                util::ensure_private_dir(&recovery, true).unwrap();
                write(&recovery, "incident.json", &json!({"phase":"pending"}));
                "unresolved recovery"
            }
            "descendants" => {
                f.env.child_processes[0].descendants.push(ProcessRecord {
                    pid: 424243,
                    start_time: Some(43),
                });
                "recorded descendants"
            }
            "broker-identity" => {
                f.env.child_processes[0].start_time = Some(99);
                "not registered"
            }
            _ => unreachable!(),
        };
        f.retained(reason);
    }
}

#[test]
fn actual_recovery_observer_unknown_incidents_retain_and_handled_incidents_allow_selection() {
    for phase in [
        json!("pending"),
        json!("unknown"),
        J::Null,
        json!("handled"),
    ] {
        let f = Fixture::new();
        let recovery = f.coordinator.join("recovery-observer");
        util::ensure_private_dir(&recovery, true).unwrap();
        write(&recovery, "incident.json", &json!({"phase":phase}));
        if phase == "handled" {
            assert_eq!(f.plan().entries.len(), 1);
        } else {
            f.retained("unresolved recovery");
        }
        assert!(recovery.join("incident.json").is_file());
    }
    let f = Fixture::new();
    // An arbitrary top-level incident document is not the recovery observer's
    // authority. It must not create an unrelated recovery obligation.
    write(&f.coordinator, "incident.json", &json!({"phase":"pending"}));
    assert_eq!(f.plan().entries.len(), 1);
    assert!(f.coordinator.join("incident.json").is_file());
}

#[test]
fn other_runtime_cwd_packets_turns_and_receiver_references_preserve_review_clone() {
    for name in [
        "runtime.json",
        "creation.json",
        "turn.json",
        "receiver.json",
    ] {
        let mut f = Fixture::new();
        let path = f.review();
        let other = f.env.temp_root.join("rt/unrelated-worker");
        util::ensure_private_dir(&other, true).unwrap();
        let value = match name {
            "runtime.json" => json!({"cwd":path}),
            "creation.json" => json!({"phase":"unknown","packet":{"context":{"cwd":path}}}),
            "turn.json" => {
                json!({"phase":"in_progress","packet":{"attachments":[{"path":path.join("result")}]}})
            }
            "receiver.json" => json!({"state":{"evidence":[path]}}),
            _ => unreachable!(),
        };
        write(&other, name, &value);
        let before = fs::read(other.join(name)).unwrap();
        let plan = f.plan();
        assert_eq!(plan.entries.len(), 1, "{name}");
        assert!(plan.entries[0].removal.is_none(), "{name}");
        assert!(plan.entries[0].review.is_some());
        assert!(path.join("result").is_file());
        assert_eq!(fs::read(other.join(name)).unwrap(), before);
        assert!(!other.join(MARKER).exists());
    }
}

#[test]
fn unknown_other_runtime_documents_and_conservative_text_matches_never_grant_removal() {
    let mut f = Fixture::new();
    let path = f.review();
    let other = f.env.temp_root.join("rt/unrelated-worker");
    util::ensure_private_dir(&other, true).unwrap();
    // A string prefix may refer to a similarly named checkout. The bounded
    // reference scan deliberately preserves that false positive for inspection.
    write(
        &other,
        "runtime.json",
        &json!({"cwd":format!("{}-other",path.display())}),
    );
    let plan = f.plan();
    assert_eq!(plan.entries.len(), 1);
    assert!(plan.entries[0].removal.is_none());
    assert!(path.join("result").is_file());
    // A known, unrelated path must permit reclamation rather than retaining
    // every directory merely because another runtime exists.
    write(
        &other,
        "runtime.json",
        &json!({"cwd":f.env.workspace_root.join("unrelated-checkout")}),
    );
    assert!(f.plan().entries[0].removal.is_some());
    fs::write(other.join("creation.json"), "unknown partial JSON {").unwrap();
    let plan = f.plan();
    assert!(plan.entries.is_empty());
    assert_eq!(plan.retained.len(), 1);
    assert!(path.join("result").is_file());
    assert_eq!(
        fs::read_to_string(other.join("creation.json")).unwrap(),
        "unknown partial JSON {"
    );
    assert!(!f.runtime.join(MARKER).exists());
}

#[test]
fn unreferenced_ready_review_gets_exact_removal_authority_and_references_keep_it() {
    let mut f = Fixture::new();
    let path = f.review();
    let identity = f.env.review_workspaces[ASSIGNMENT].identity.unwrap();
    let plan = f.plan();
    let removal = plan.entries[0].removal.as_ref().unwrap();
    assert_eq!(removal.path, path);
    assert_eq!(removal.identity, identity);
    assert!(path.join("result").is_file());
    f.state["evidence"] =
        json!({"nested":[format!("inspect {} before continuing",path.display())]});
    let plan = f.plan();
    assert_eq!(plan.entries.len(), 1);
    assert!(plan.entries[0].review.is_some());
    assert!(plan.entries[0].removal.is_none());
    assert!(path.join("result").is_file());
    f.state.as_object_mut().unwrap().remove("evidence");
    let mut dependent = f.env.review_workspaces[ASSIGNMENT].clone();
    dependent.binding["assignment"] = json!("later-review");
    dependent.path = f.env.workspace_root.join("reviews/later-review");
    dependent.runtime = f.env.temp_root.join("rt/later-review");
    dependent.source = path.join("nested-source");
    f.env
        .review_workspaces
        .insert("later-review".into(), dependent);
    let plan = f.plan();
    assert!(plan.entries[0].removal.is_none());
    assert!(path.join("result").is_file());
}

#[test]
fn review_identity_phase_and_original_binding_changes_are_retained() {
    for fault in ["identity", "phase", "binding", "wait"] {
        let mut f = Fixture::new();
        let path = f.review();
        let reason = match fault {
            "identity" => {
                let original = path.with_file_name("saved-original");
                fs::rename(&path, &original).unwrap();
                util::ensure_private_dir(&path, true).unwrap();
                "identity changed"
            }
            "phase" => {
                f.env.review_workspaces.get_mut(ASSIGNMENT).unwrap().phase =
                    task_env::review::Phase::Cloning;
                "unsettled"
            }
            "binding" => {
                f.env.review_workspaces.get_mut(ASSIGNMENT).unwrap().binding["candidate"] =
                    json!("later-candidate");
                "review"
            }
            "wait" => {
                f.env.review_workspaces.get_mut(ASSIGNMENT).unwrap().wait = "another-result".into();
                "review"
            }
            _ => unreachable!(),
        };
        f.retained(reason);
        assert!(path.is_dir());
    }
}

#[test]
fn collection_receipt_exact_replay_preserves_receipt_and_mismatch_fails_closed() {
    let f = Fixture::new();
    let path = f.runtime.join(RECEIPT);
    let before = fs::read(&path).unwrap();
    let inode = fs::metadata(&path).unwrap().ino();
    collection(&f.coordinator, &f.owner, &f.collected).unwrap();
    assert_eq!(fs::read(&path).unwrap(), before);
    assert_eq!(fs::metadata(&path).unwrap().ino(), inode);
    for fault in ["binding", "wait", "coordinator", "owner"] {
        let mut receipt: J = serde_json::from_slice(&before).unwrap();
        match fault {
            "binding" => receipt["binding"]["epoch"] = json!(999),
            "wait" => receipt["wait"] = json!("later-result"),
            "coordinator" => receipt["coordinator"] = json!("/foreign/coordinator"),
            "owner" => receipt["owner"]["instance_id"] = json!("foreign-instance"),
            _ => unreachable!(),
        }
        write(&f.runtime, RECEIPT, &receipt);
        let changed = fs::read(&path).unwrap();
        assert!(collection(&f.coordinator, &f.owner, &f.collected).is_err());
        assert_eq!(fs::read(&path).unwrap(), changed);
    }
}

#[test]
fn collection_requires_signalled_original_sibling_runtime_and_owner() {
    let f = Fixture::new();
    fs::remove_file(f.runtime.join(RECEIPT)).unwrap();
    let mut pending = f.collected.clone();
    pending["collection"]["phase"] = json!("collecting");
    collection(&f.coordinator, &f.owner, &pending).unwrap();
    assert!(!f.runtime.join(RECEIPT).exists());
    let mut changed = f.collected.clone();
    changed["result"]["contentItems"][0]["text"] =
        json!(json!({"runtime":f.env.workspace_root,"wait":WAIT}).to_string());
    assert!(collection(&f.coordinator, &f.owner, &changed).is_err());
    assert!(!f.runtime.join(RECEIPT).exists());
    f.edit("runtime.json", |v| {
        v["lifecycle_owner"]["epoch"] = json!(999)
    });
    assert!(collection(&f.coordinator, &f.owner, &f.collected).is_err());
    assert!(!f.runtime.join(RECEIPT).exists());
}
