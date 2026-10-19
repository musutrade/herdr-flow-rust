//! Offline regression checks for the batch refactor: dependency validation,
//! bounded offer with dependencies, and public stall/repair/action edges.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use herdr_flow::batch;
use herdr_flow::ci::watch::GitHubApi;
use herdr_flow::error::FlowError;
use herdr_flow::model::{self, J};
use herdr_flow::Result;
use serde_json::json;
use tempfile::TempDir;

struct Fake {
    get: BTreeMap<String, J>,
}

impl Fake {
    fn new() -> Self {
        Self {
            get: BTreeMap::new(),
        }
    }

    fn with(mut self, path: &str, value: J) -> Self {
        self.get.insert(path.to_string(), value);
        self
    }
}

impl GitHubApi for Fake {
    fn get(&self, suffix: &str) -> Result<J> {
        self.get
            .get(suffix)
            .cloned()
            .ok_or_else(|| FlowError::watch(format!("unexpected GET {suffix}")))
    }
}

fn frozen_member(head: &str, epoch: i64, original_base: &str) -> J {
    json!({
        "task": "member",
        "head": head,
        "epoch": epoch,
        "original_base": original_base,
        "dependencies": [],
    })
}

fn frozen_task(head: &str, epoch: i64) -> J {
    json!({
        "task": "member",
        "candidate": head,
        "epoch": epoch,
        "control": "running",
        "phase": "blocked",
        "pending": [],
        "findings": {},
        "writer": null
    })
}

#[test]
fn dependencies_accepts_declared_base_and_rejects_changed_member() {
    let s = json!({"tasks": {"member": frozen_task("head-1", 1)}});
    let lane = json!({});
    let batch = json!({"base": "base-1", "members": [frozen_member("head-1", 1, "base-1")]});
    batch::validation::dependencies(&s, &lane, &batch, &Fake::new()).unwrap();

    let changed = json!({"tasks": {"member": frozen_task("other", 1)}});
    let error = batch::validation::dependencies(&changed, &lane, &batch, &Fake::new()).unwrap_err();
    assert!(format!("{error:?}").contains("frozen member identity"));
}

#[test]
fn dependencies_accepts_declared_landing_and_rejects_pinned_mismatch() {
    let mut member = frozen_member("member-head", 1, "target-base");
    member["dependencies"] = json!([{"task": "dep", "head": "dep-head"}]);
    let batch = json!({"base": "target-base", "members": [member]});
    let lane = json!({"repo": "o/r", "base_ref": "main"});
    let tasks = json!({
        "member": frozen_task("member-head", 1),
        "dep": {
            "task": "dep",
            "candidate": "dep-head",
            "control": "running",
            "batch_delivery": {
                "head": "dep-head",
                "landing": {
                    "head": "dep-head",
                    "commit": "dep-commit",
                    "repo": "o/r",
                    "base_ref": "main"
                }
            }
        }
    });
    let state = json!({"tasks": tasks});
    let api = Fake::new().with(
        "compare/dep-commit...target-base",
        json!({"status": "ahead", "merge_base_commit": {"sha": "dep-commit"}}),
    );
    batch::validation::dependencies(&state, &lane, &batch, &api).unwrap();

    let mut pinned = batch.clone();
    pinned["members"][0]["dependencies"][0]["landing"] = json!({"commit": "other-commit"});
    let error = batch::validation::dependencies(&state, &lane, &pinned, &Fake::new()).unwrap_err();
    assert!(format!("{error:?}").contains("pinned dependency landing changed"));
}

struct Repo {
    _tmp: TempDir,
    root: PathBuf,
    base: String,
    first: String,
    second: String,
}

impl Repo {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        git(&root, &["init", "-q"]);
        git(&root, &["config", "user.email", "test@example.invalid"]);
        git(&root, &["config", "user.name", "Test"]);
        std::fs::write(root.join("a"), "initial").unwrap();
        git(&root, &["add", "a"]);
        git(&root, &["commit", "-qm", "initial"]);
        let base = output(&root, &["rev-parse", "HEAD"]);
        git(
            &root,
            &["remote", "add", "origin", "https://github.com/o/r.git"],
        );
        git(&root, &["checkout", "-qb", "feature"]);
        std::fs::write(root.join("a"), "feature").unwrap();
        git(&root, &["add", "a"]);
        git(&root, &["commit", "-qm", "feature"]);
        let first = output(&root, &["rev-parse", "HEAD"]);
        std::fs::write(root.join("b"), "successor").unwrap();
        git(&root, &["add", "b"]);
        git(&root, &["commit", "-qm", "successor"]);
        let second = output(&root, &["rev-parse", "HEAD"]);
        Self {
            _tmp: tmp,
            root,
            base,
            first,
            second,
        }
    }
}

fn git(cwd: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .status()
        .unwrap();
    assert!(status.success(), "git {:?}", args);
}

fn output(cwd: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap();
    assert!(output.status.success(), "git {:?}", args);
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

fn task(name: &str, candidate: &str) -> J {
    json!({
        "task": name,
        "epoch": 1,
        "assignment": format!("{name}-assignment"),
        "candidate": candidate,
        "goal": "ship",
        "owner": "dev",
        "state_version": 1,
        "phase": "blocked",
        "control": "running",
        "wait": format!("{name}-wait"),
        "next_action": {"kind": "blocked", "owner": "dev", "reason": "done", "condition": "publish"},
        "acceptance": {"first": {"description": "first", "required": true, "evidence": null}},
        "authorization": {"publish": {"action": "publication", "source": "user"}},
        "pending": [],
        "operations": {},
        "findings": {},
        "history": [],
        "created_at": 1.0,
        "apply_timeout": 600,
        "writer": null
    })
}

fn event(name: &str, candidate: &str) -> J {
    json!({
        "id": format!("{name}-event"),
        "wait": format!("{name}-wait"),
        "task": name,
        "epoch": 1,
        "owner": "dev",
        "outcome": "complete",
        "summary": "done",
        "applied": true,
        "acknowledged": true,
        "observed_at": 5.0,
        "binding": {
            "task": name,
            "epoch": 1,
            "assignment": format!("{name}-assignment"),
            "candidate": candidate
        }
    })
}

#[test]
fn offer_accepts_dependency_on_earlier_pending_member() {
    let repo = Repo::new();
    let lane_task = task("lane", &repo.base);
    let first_task = task("m1", &repo.first);
    let second_task = task("m2", &repo.second);
    let mut state = json!({
        "version": 1,
        "id": "q1",
        "guard_dir": null,
        "receiver": null,
        "poll_seconds": 30,
        "notify": false,
        "stopped": false,
        "waits": {},
        "events": {},
        "deliveries": {},
        "active": null,
        "delivery_status": "disabled",
        "tasks": {"lane": lane_task, "m1": first_task, "m2": second_task}
    });
    state["events"]["m1-event"] = event("m1", &repo.first);
    state["events"]["m2-event"] = event("m2", &repo.second);
    let integration = repo._tmp.path().join("integration");
    let init_spec = json!({
        "task": "lane",
        "state_version": 1,
        "repo": "o/r",
        "host": "github.com",
        "base_ref": "main",
        "workspace_root": integration.display().to_string(),
        "capacity": 2,
        "gate": {"verifier": ["/bin/true"], "policy": "policy-1", "check_name": "trusted-gate", "app_id": 123},
        "max_repairs": 2
    });
    let lane = batch::lane::init(&mut state, &init_spec, 10.0).unwrap();
    let version = model::int(&lane, "state_version").unwrap();
    let first_spec = json!({
        "task": "lane",
        "state_version": version,
        "member": {
            "task": "m1",
            "state_version": 1,
            "event": "m1-event",
            "checkout": repo.root.display().to_string(),
            "original_base": repo.base,
            "head": repo.first,
            "own_commits": [repo.first],
            "acceptance": ["first"],
            "dependencies": [],
            "evidence": "/tmp/evidence.json"
        }
    });
    let offered = batch::lane::offer(&mut state, &first_spec, 11.0).unwrap();
    let version = model::int(&offered, "state_version").unwrap();
    let second_spec = json!({
        "task": "lane",
        "state_version": version,
        "member": {
            "task": "m2",
            "state_version": 1,
            "event": "m2-event",
            "checkout": repo.root.display().to_string(),
            "original_base": repo.first,
            "head": repo.second,
            "own_commits": [repo.second],
            "acceptance": ["first"],
            "dependencies": [{"task": "m1", "head": repo.first}],
            "evidence": "/tmp/evidence.json"
        }
    });
    let offered = batch::lane::offer(&mut state, &second_spec, 12.0).unwrap();
    let pending = model::array_field(model::get(&offered, "lane").unwrap(), "pending")
        .cloned()
        .unwrap();
    assert_eq!(pending.len(), 2);
    assert_eq!(pending[1]["dependencies"][0]["task"], json!("m1"));
    assert_eq!(pending[1]["dependencies"][0]["head"], json!(repo.first));
}

#[test]
fn stall_and_repair_guard_edges() {
    let active = json!({"phase": "integrating", "progress_deadline": 5.0});
    let blocked = json!({"phase": "blocked", "lane": {"active": active}});
    assert_eq!(
        batch::lane::stall(&blocked, 4.0),
        None,
        "an unexpired batch must not stall"
    );
    assert_eq!(
        batch::lane::stall(&blocked, 6.0),
        Some("batch_successor_unaccepted".to_string())
    );
    let uncertain = json!({
        "phase": "blocked",
        "lane": {"active": {"phase": "gate_uncertain", "progress_deadline": 5.0}}
    });
    assert_eq!(
        batch::lane::stall(&uncertain, 6.0),
        Some("batch_operation_unreconciled".to_string())
    );

    let state = json!({});
    let no_lane = json!({"phase": "blocked"});
    batch::lane::check_repair(&state, &no_lane, &json!({}), 0.0, false).unwrap();

    let batch_state = json!({
        "id": "b1",
        "base": "base",
        "revision": 1,
        "phase": "integration_conflict",
        "integration": {"conflict": null},
        "gate": null
    });
    let token = batch::lane::repair_token(&batch_state);
    let task_state = json!({
        "task": "lane",
        "candidate": "cand",
        "lane": {"active": batch_state}
    });
    let op = json!({
        "intent": {
            "writes": true,
            "batch_repair": {"token": token, "candidate": "cand", "base": "base"}
        }
    });
    batch::lane::check_repair(&state, &task_state, &op, 0.0, false).unwrap();
    let stale = json!({
        "intent": {
            "writes": true,
            "batch_repair": {"token": "stale", "candidate": "cand", "base": "base"}
        }
    });
    assert!(batch::lane::check_repair(&state, &task_state, &stale, 0.0, false).is_err());

    let error = batch::action(
        &mut state.clone(),
        "batch-unknown",
        &json!({}),
        0.0,
        &mut |_s| Ok(()),
    )
    .unwrap_err();
    assert!(format!("{error:?}").contains("unknown batch action"));
}

fn verifier_script(directory: &Path, name: &str, response: &J) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = directory.join(name);
    let script = format!(
        "#!/bin/sh\ncat >/dev/null\nprintf '%s\\n' '{}'\n",
        serde_json::to_string(response).unwrap()
    );
    std::fs::write(&path, script).unwrap();
    let mut permissions = std::fs::metadata(&path).unwrap().permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&path, permissions).unwrap();
    path
}

#[test]
fn gate_query_and_quiescent_accept_valid_receipts() {
    let tmp = tempfile::tempdir().unwrap();
    let verifier = tmp.path().join("verifier");
    std::fs::create_dir_all(&verifier).unwrap();
    let lane = json!({
        "repo": "o/r",
        "host": "github.com",
        "base_ref": "main",
        "gate": {"verifier": [verifier.join("verify").display().to_string()], "policy": "policy-1", "check_name": "trusted-gate", "app_id": 123}
    });
    let batch = json!({
        "base": "base-1",
        "snapshot": {"head": "head-1", "tree": "tree-1"},
        "gate": {"operation": "op-1"}
    });
    let binding = batch::gate::binding(&lane, &batch).unwrap();
    let receipt = json!({
        "version": 1,
        "binding": binding,
        "operation": "op-1",
        "valid": true,
        "coverage_complete": true,
        "app_id": 123,
        "check_name": "trusted-gate",
        "decision": "allow",
        "expires_at": 9999999999.0,
        "run": "run-1",
        "external_id": "external-1",
        "evidence": "evidence-1"
    });
    verifier_script(&verifier, "verify", &receipt);
    let verified = batch::gate::query(&lane, &batch, Some(&json!("reference-1")), 100.0).unwrap();
    assert_eq!(
        model::string(&verified, "decision").as_deref(),
        Some("allow")
    );

    let inspection = json!({
        "version": 1,
        "binding": binding,
        "operation": "op-1",
        "status": "completed",
        "quiescent": true,
        "evidence": "inspection-1"
    });
    verifier_script(&verifier, "verify", &inspection);
    let inspected = batch::gate::quiescent(&lane, &batch).unwrap();
    assert_eq!(
        model::string(&inspected, "status").as_deref(),
        Some("completed")
    );
}

fn guard_case(active: J) -> (J, J) {
    let mut lane_task = task("lane", "candidate");
    lane_task["lane"] = json!({
        "repo": "o/r",
        "host": "github.com",
        "base_ref": "main",
        "workspace_root": "/tmp/herdr-refactor-guard/integration",
        "capacity": 2,
        "gate": {"verifier": ["/bin/true"], "policy": "policy-1", "check_name": "trusted-gate", "app_id": 123},
        "pending": [],
        "active": active,
        "history": [],
        "sequence": 1,
        "older_batches": 0,
        "repairs": {},
        "max_repairs": 2,
        "gate_operations": {},
        "progress_timeout": 600
    });
    let state = json!({
        "version": 1,
        "id": "q1",
        "guard_dir": null,
        "receiver": null,
        "poll_seconds": 30,
        "notify": false,
        "stopped": false,
        "waits": {},
        "events": {},
        "deliveries": {},
        "active": null,
        "delivery_status": "disabled",
        "tasks": {"lane": lane_task}
    });
    let spec = json!({"task": "lane", "state_version": 1, "batch": "b1"});
    (state, spec)
}

#[test]
fn worklist_phase_guards_reject_mismatched_batches() {
    let error_contains = |result: Result<J>, text: &str| {
        let error = result.unwrap_err();
        assert!(
            format!("{error:?}").contains(text),
            "expected {text:?}, got {error:?}"
        );
    };
    let active = json!({"id": "b1", "base": "base", "phase": "snapshot_frozen"});
    let (mut state, spec) = guard_case(active);
    error_contains(
        batch::lane::integrate(&mut state, &spec, 1.0, &mut |_s| Ok(()), None),
        "not ready for integration",
    );

    let (mut state, spec) =
        guard_case(json!({"id": "b1", "base": "base", "phase": "integration_prepared"}));
    error_contains(
        batch::lane::snapshot(&mut state, &spec, 1.0, None),
        "final snapshot",
    );

    let (mut state, spec) = guard_case(json!({"id": "b1", "base": "base", "phase": "integrating"}));
    error_contains(
        batch::lane::gate_begin(&mut state, &spec, 1.0, None),
        "gate may already have executed",
    );

    let (mut state, spec) = guard_case(json!({"id": "b1", "base": "base", "phase": "integrating"}));
    error_contains(
        batch::lane::gate_record(&mut state, &spec, 1.0, None),
        "no existing gate operation",
    );

    let (mut state, spec) = guard_case(json!({"id": "b1", "base": "base", "phase": "integrated"}));
    error_contains(
        batch::lane::repair(&mut state, &spec, 1.0),
        "batch repair requires",
    );

    let (mut state, spec) = guard_case(json!({"id": "b1", "base": "base", "phase": "integrated"}));
    error_contains(
        batch::lane::repair_finish(&mut state, &spec, 1.0, &mut |_s| Ok(())),
        "no prepared private repair update",
    );

    let (mut state, spec) =
        guard_case(json!({"id": "b1", "base": "base", "repair_update": {"parent": "p"}}));
    error_contains(
        batch::lane::rebase(&mut state, &spec, 1.0, None),
        "reconcile uncertain repair/publication",
    );
}
