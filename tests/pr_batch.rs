//! PR provenance/publication and bounded batch integration over real Git.

pub mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use herdr_flow::batch;
use herdr_flow::ci::watch::GitHubApi;
use herdr_flow::model::{self, J};
use herdr_flow::pr;
use herdr_flow::util;
use serde_json::{json, Value};
use tempfile::TempDir;

const BASE_SHA_PLACEHOLDER: &str = "bbbb";

struct Repo {
    _tmp: TempDir,
    root: PathBuf,
    base: String,
    head: String,
}

impl Repo {
    fn new() -> Self {
        let tmp = common::tempdir().unwrap();
        let root = tmp.path().join("repo");
        common::create_dir_all(&root).unwrap();
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
        let head = output(&root, &["rev-parse", "HEAD"]);
        Self {
            _tmp: tmp,
            root,
            base,
            head,
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

fn task(repo: &Repo) -> J {
    json!({
        "task": "o/r#1",
        "epoch": 1,
        "assignment": "a1",
        "candidate": repo.head,
        "goal": "ship",
        "owner": "dev",
        "state_version": 1,
        "phase": "blocked",
        "control": "running",
        "wait": "step1",
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

fn queue_state(task: J) -> J {
    json!({
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
        "tasks": {"o/r#1": task}
    })
}

fn prepare_spec(repo: &Repo, version: i64) -> J {
    json!({
        "task": "o/r#1",
        "state_version": version,
        "checkout": repo.root.display().to_string(),
        "repo": "o/r",
        "host": "github.com",
        "original_base": repo.base,
        "head": repo.head,
        "own_commits": [repo.head],
        "base_ref": "main",
        "head_ref": "feature",
        "issue": 7,
        "target_base": repo.base,
        "dependencies": []
    })
}

struct Fake {
    get: BTreeMap<String, J>,
}

impl Fake {
    fn new() -> Self {
        Self {
            get: BTreeMap::new(),
        }
    }
    fn with(mut self, path: &str, value: Value) -> Self {
        self.get.insert(path.to_string(), value);
        self
    }
}

impl GitHubApi for Fake {
    fn get(&self, suffix: &str) -> herdr_flow::Result<J> {
        self.get
            .get(suffix)
            .cloned()
            .ok_or_else(|| herdr_flow::FlowError::watch(format!("unexpected GET {suffix}")))
    }
}

fn pr_json(repo: &Repo, merged: bool) -> J {
    json!({
        "number": 7,
        "head": {"sha": repo.head, "ref": "feature", "repo": {"full_name": "o/r"}},
        "base": {"sha": repo.base, "ref": "main", "repo": {"full_name": "o/r"}},
        "state": "open",
        "merged": merged,
        "mergeable": true,
        "auto_merge": {"enabled": true},
        "merge_commit_sha": repo.head
    })
}

#[test]
fn pr_prepare_begin_reconcile_and_landing() {
    let repo = Repo::new();
    let mut s = queue_state(task(&repo));
    let prepared = pr::prepare(&mut s, &prepare_spec(&repo, 1), 10.0).unwrap();
    assert_eq!(
        model::string(&prepared, "phase").as_deref(),
        Some("blocked")
    );
    assert_eq!(
        model::dig_str(&prepared, &["pr", "phase"]).as_deref(),
        Some("publication_prepared")
    );
    let version = model::int(&prepared, "state_version").unwrap();
    let begun = pr::publication_begin(
        &mut s,
        &json!({"task": "o/r#1", "state_version": version, "authorization": "publish"}),
        11.0,
    )
    .unwrap();
    assert_eq!(
        model::dig_str(&begun, &["pr", "publication", "phase"]).as_deref(),
        Some("sending")
    );
    let version = model::int(&begun, "state_version").unwrap();
    let pull = pr_json(&repo, false);
    let api = Fake::new()
        .with(
            "pulls?state=all&head=o%3Afeature&base=main&per_page=100&page=1",
            json!([pull.clone()]),
        )
        .with("pulls/7", pull);
    let reconciled = pr::reconcile(
        &mut s,
        &json!({"task": "o/r#1", "state_version": version}),
        12.0,
        Some(&api),
    )
    .unwrap();
    assert_eq!(
        model::dig_str(&reconciled, &["pr", "phase"]).as_deref(),
        Some("registered")
    );
    let version = model::int(&reconciled, "state_version").unwrap();
    let merged = pr_json(&repo, true);
    let api = Fake::new()
        .with("pulls/7", merged)
        .with("branches/main", json!({"commit": {"sha": repo.head}}))
        .with(
            &format!("compare/{}...{}", repo.head, repo.head),
            json!({"status": "identical", "merge_base_commit": {"sha": repo.head}}),
        );
    let landed = pr::reconcile(
        &mut s,
        &json!({"task": "o/r#1", "state_version": version}),
        13.0,
        Some(&api),
    )
    .unwrap();
    assert_eq!(
        model::dig_str(&landed, &["pr", "phase"]).as_deref(),
        Some("merged")
    );
    assert_eq!(
        model::dig_str(&landed, &["pr", "landing", "commit"]).as_deref(),
        Some(repo.head.as_str())
    );
    let artifact = json!({"adapter":"git","id":repo.head});
    assert!(matches!(
        herdr_flow::delivery::publication::verify(&s, &landed, &landed, &artifact, Some(&api))
            .unwrap(),
        herdr_flow::delivery::Observation::Delivered(_)
    ));
    let wrong = json!({"adapter":"git","id":"0".repeat(40)});
    assert!(
        herdr_flow::delivery::publication::verify(&s, &landed, &landed, &wrong, Some(&api))
            .is_err()
    );
    let mut member = landed.clone();
    member["pr"] = Value::Null;
    let mut mapping = landed["pr"]["landing"].clone();
    mapping["head"] = wrong["id"].clone();
    mapping["batch_head"] = artifact["id"].clone();
    member["batch_delivery"] = json!({"phase":"landed","landing":mapping});
    assert!(matches!(
        herdr_flow::delivery::publication::verify(&s, &member, &landed, &wrong, Some(&api))
            .unwrap(),
        herdr_flow::delivery::Observation::Delivered(_)
    ));
    member["batch_delivery"]["phase"] = json!("pending");
    assert!(
        herdr_flow::delivery::publication::verify(&s, &member, &landed, &wrong, Some(&api))
            .is_err()
    );
    let _ = BASE_SHA_PLACEHOLDER;
}

#[test]
fn batch_offer_freeze_integrate_and_snapshot() {
    let repo = Repo::new();
    let tmp = common::tempdir().unwrap();
    let integration_root = tmp.path().join("integration");
    let mut lane_task = task(&repo);
    lane_task["task"] = json!("lane");
    lane_task["candidate"] = json!(repo.base);
    lane_task["acceptance"] = json!({});
    let mut member = task(&repo);
    member["task"] = json!("o/r#2");
    member["epoch"] = json!(1);
    member["assignment"] = json!("m1");
    member["candidate"] = json!(repo.head);
    member["wait"] = json!("step2");
    member["acceptance"] =
        json!({"first": {"description": "first", "required": true, "evidence": null}});
    let mut s = queue_state(lane_task.clone());
    s["tasks"] = json!({"lane": lane_task, "o/r#2": member.clone()});
    s["events"]["e1"] = json!({
        "id": "e1",
        "wait": "step2",
        "task": "o/r#2",
        "epoch": 1,
        "owner": "dev",
        "outcome": "complete",
        "summary": "done",
        "applied": true,
        "acknowledged": true,
        "observed_at": 5.0,
        "binding": {"task": "o/r#2", "epoch": 1, "assignment": "m1", "candidate": repo.head}
    });
    let gate = json!({
        "verifier": ["/bin/true"],
        "policy": "policy-1",
        "check_name": "trusted-gate",
        "app_id": 123
    });
    let init_spec = json!({
        "task": "lane",
        "state_version": 1,
        "repo": "o/r",
        "host": "github.com",
        "base_ref": "main",
        "workspace_root": integration_root.display().to_string(),
        "capacity": 2,
        "gate": gate,
        "max_repairs": 2
    });
    let lane = batch::action(&mut s, "batch-init", &init_spec, 10.0, &mut |_s| Ok(())).unwrap();
    assert_eq!(
        model::dig_str(&lane, &["lane", "repo"]).as_deref(),
        Some("o/r")
    );
    let version = model::int(&lane, "state_version").unwrap();
    let offer_spec = json!({
        "task": "lane",
        "state_version": version,
        "member": {
            "task": "o/r#2",
            "state_version": 1,
            "event": "e1",
            "checkout": repo.root.display().to_string(),
            "original_base": repo.base,
            "head": repo.head,
            "own_commits": [repo.head],
            "acceptance": ["first"],
            "dependencies": [],
            "evidence": "/tmp/evidence.json"
        }
    });
    let mut full = s.clone();
    full["tasks"]["lane"]["lane"]["pending"] = json!([{}, {}]);
    let unchanged = full.clone();
    assert!(batch::action(
        &mut full,
        "batch-offer",
        &offer_spec,
        11.0,
        &mut |_s| Ok(())
    )
    .is_err());
    assert_eq!(full, unchanged, "capacity refusal must not mint a receipt");

    let unchanged = s.clone();
    let mut invalid = offer_spec.clone();
    invalid["member"]["evidence"] = json!("");
    assert!(batch::action(&mut s, "batch-offer", &invalid, 11.0, &mut |_s| Ok(())).is_err());
    assert_eq!(s, unchanged, "validation failure must not mint a receipt");

    let offered =
        batch::action(&mut s, "batch-offer", &offer_spec, 11.0, &mut |_s| Ok(())).unwrap();
    let receipt = s["tasks"]["o/r#2"]["batch_delivery"]["receipt_id"]
        .as_str()
        .expect("successful intake has a receipt")
        .to_owned();
    assert!(!receipt.is_empty());
    assert_eq!(offered["lane"]["pending"][0]["receipt_id"], receipt);
    // A JSON round trip keeps the exact receipt; a repeated offer cannot replace it.
    s = serde_json::from_slice(&serde_json::to_vec(&s).unwrap()).unwrap();
    let unchanged = s.clone();
    let mut duplicate = offer_spec.clone();
    duplicate["state_version"] = s["tasks"]["lane"]["state_version"].clone();
    duplicate["member"]["state_version"] = s["tasks"]["o/r#2"]["state_version"].clone();
    assert!(batch::action(&mut s, "batch-offer", &duplicate, 11.5, &mut |_s| Ok(())).is_err());
    assert_eq!(s, unchanged);
    assert_eq!(
        model::array_field(model::get(&offered, "lane").unwrap(), "pending").map(Vec::len),
        Some(1)
    );
    let version = model::int(&offered, "state_version").unwrap();
    let freeze_spec = json!({"task": "lane", "state_version": version});
    let api = Fake::new().with("branches/main", json!({"commit": {"sha": repo.base}}));
    let frozen = batch::lane::freeze(&mut s, &freeze_spec, 12.0, Some(&api)).unwrap();
    assert_eq!(s["tasks"]["o/r#2"]["batch_delivery"]["receipt_id"], receipt);
    assert_eq!(
        frozen["lane"]["active"]["members"][0]["receipt_id"],
        receipt
    );
    let lane_state = model::get(&frozen, "lane").unwrap();
    let active = model::get(lane_state, "active").unwrap();
    assert_eq!(
        model::string(active, "phase").as_deref(),
        Some("integration_prepared")
    );
    let batch_id = model::string(active, "id").unwrap();
    assert!(model::array_field(active, "corrections").is_some());
    assert!(model::array_field(active, "versions").is_some());
    let workspace = PathBuf::from(model::string(active, "workspace").unwrap());
    // Pre-populate the private object store so integration never needs network.
    common::create_dir_all(&workspace).unwrap();
    util::atomic_write_json(
        &workspace.join("owner.json"),
        &json!({"batch": batch_id, "repo": "o/r", "base": repo.base}),
    )
    .unwrap();
    let bare = workspace.join("objects.git");
    git(&workspace, &["init", "--bare", bare.to_str().unwrap()]);
    git(
        &bare,
        &["remote", "add", "origin", "https://github.com/o/r.git"],
    );
    git(
        &bare,
        &[
            "fetch",
            "--no-tags",
            repo.root.to_str().unwrap(),
            &repo.base,
        ],
    );
    git(
        &bare,
        &[
            "fetch",
            "--no-tags",
            repo.root.to_str().unwrap(),
            &repo.head,
        ],
    );
    let frozen_version = model::int(&frozen, "state_version").unwrap();
    let integrate_spec =
        json!({"task": "lane", "state_version": frozen_version, "batch": batch_id});
    let integrated =
        batch::lane::integrate(&mut s, &integrate_spec, 13.0, &mut |_s| Ok(()), Some(&api))
            .unwrap();
    let lane_state = model::get(&integrated, "lane").unwrap();
    assert_eq!(
        model::dig_str(lane_state, &["active", "phase"]).as_deref(),
        Some("integrated"),
        "{integrated}"
    );
    let version = model::int(&integrated, "state_version").unwrap();
    assert!(
        version > frozen_version,
        "integration must advance the task version"
    );
    let snapshot_spec =
        json!({"task": "lane", "state_version": version, "batch": batch_id, "issue": 7});
    let snapshotted = batch::lane::snapshot(&mut s, &snapshot_spec, 14.0, Some(&api)).unwrap();
    let lane_state = model::get(&snapshotted, "lane").unwrap();
    assert_eq!(
        model::dig_str(lane_state, &["active", "phase"]).as_deref(),
        Some("snapshot_frozen"),
        "{snapshotted}"
    );
    assert!(model::dig(&snapshotted, &["pr", "head"]).is_some());
    assert_eq!(s["tasks"]["o/r#2"]["batch_delivery"]["receipt_id"], receipt);
    assert_eq!(
        snapshotted["lane"]["active"]["members"][0]["receipt_id"],
        receipt
    );
}

#[test]
fn batch_gate_repair_imports_verified_range_before_private_ref_update() {
    let repo = Repo::new();
    let tmp = common::tempdir().unwrap();
    let workspace = tmp.path().join("batch-repair");
    common::create_dir(&workspace).unwrap();
    let bare = workspace.join("objects.git");
    git(
        &workspace,
        &["init", "--bare", "-q", bare.to_str().unwrap()],
    );
    git(
        &bare,
        &["remote", "add", "origin", "https://github.com/o/r.git"],
    );
    git(
        &bare,
        &[
            "fetch",
            "--no-tags",
            repo.root.to_str().unwrap(),
            &repo.base,
        ],
    );
    git(&bare, &["update-ref", "refs/herdr/candidate", &repo.base]);

    let mut owner = task(&repo);
    owner["candidate"] = json!(repo.base);
    owner["lane"] = json!({
        "repo": "o/r",
        "host": "github.com",
        "base_ref": "main",
        "workspace_root": tmp.path(),
        "max_repairs": 2,
        "active": {
            "id": "batch-repair",
            "base": repo.base,
            "phase": "gate_failed",
            "workspace": workspace,
            "members": [],
            "versions": [],
            "corrections": [],
            "integration": {"mapping": [], "offset": 0}
        }
    });
    owner["operations"]["repair-op"] = json!({
        "phase": "accepted",
        "assignment": "a1",
        "intent": {
            "workspace": repo.root,
            "batch_repair": {"batch": "batch-repair"}
        }
    });
    let mut s = queue_state(owner);
    s["events"]["repair-result"] = json!({
        "id": "repair-result",
        "applied": true,
        "outcome": "complete",
        "binding": {"task": "o/r#1", "epoch": 1, "assignment": "a1", "candidate": repo.base}
    });
    let repaired = batch::lane::repair(
        &mut s,
        &json!({
            "task": "o/r#1", "state_version": 1, "batch": "batch-repair",
            "operation": "repair-op", "event": "repair-result", "checkout": repo.root,
            "head": repo.head, "evidence": "verified isolated repair"
        }),
        20.0,
    )
    .unwrap();
    let active = &repaired["lane"]["active"];
    assert_eq!(repaired["candidate"], repo.head);
    assert_eq!(active["phase"], "integrated");
    assert_eq!(active["repair_update"]["parent"], repo.base);
    assert_eq!(active["repair_update"]["head"], repo.head);
    assert_eq!(active["repair_update"]["operation"], "repair-op");
    assert_eq!(active["corrections"][0]["own_commits"], json!([repo.head]));
    assert_eq!(
        active["corrections"][0]["evidence"],
        "verified isolated repair"
    );
    assert_eq!(
        output(&bare, &["rev-parse", "refs/herdr/candidate"]),
        repo.base
    );

    let finished = batch::action(
        &mut s,
        "batch-repair-finish",
        &json!({"task": "o/r#1", "state_version": repaired["state_version"], "batch": "batch-repair"}),
        21.0,
        &mut |_| Ok(()),
    ).unwrap();
    assert_eq!(finished, s["tasks"]["o/r#1"]);
    assert!(
        finished["state_version"].as_i64().unwrap() > repaired["state_version"].as_i64().unwrap()
    );
    assert!(finished["history"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item["action"] == "batch_repair_ref_updated"));
    assert_eq!(
        output(&bare, &["rev-parse", "refs/herdr/candidate"]),
        repo.head
    );
    assert!(s["tasks"]["o/r#1"]["lane"]["active"]
        .get("repair_update")
        .is_none());
    let api = Fake::new().with("branches/main", json!({"commit": {"sha": repo.base}}));
    let snapshot = batch::lane::snapshot(
        &mut s,
        &json!({"task": "o/r#1", "state_version": finished["state_version"],
            "batch": "batch-repair", "issue": 7}),
        22.0,
        Some(&api),
    )
    .unwrap();
    assert_eq!(snapshot["lane"]["active"]["phase"], "snapshot_frozen");
}

#[test]
fn batch_git_rejects_dangerous_options_and_commands() {
    let tmp = common::tempdir().unwrap();
    let result = batch::git::run(
        tmp.path(),
        &["-c".to_string(), "x=y".to_string(), "status".to_string()],
        None,
        None,
        &BTreeMap::new(),
        true,
    );
    assert!(result.is_err());
    let result = batch::git::run(
        tmp.path(),
        &["push".to_string()],
        None,
        None,
        &BTreeMap::new(),
        true,
    );
    assert!(result.is_err());
}

#[test]
fn batch_repair_intent_rejects_expired_observation() {
    let repo = Repo::new();
    let mut t = task(&repo);
    t["pr"] = json!({
        "number": 7,
        "head": repo.head,
        "target_base": repo.base,
        "phase": "repair_needed",
        "observation": {"token": "tok", "at": 1.0, "source": "/tmp/ci/status.json", "failures": []},
        "repairs": {},
        "max_repairs": 2,
        "checkout": repo.root.display().to_string()
    });
    let action = json!({
        "operation": "op2",
        "writes": true,
        "workspace": "/tmp/repair",
        "pr_repair": {"number": 7, "head": repo.head, "base": repo.base, "token": "tok"},
        "repair_diagnosis": {"kind": "code", "evidence": "/tmp/diagnosis"}
    });
    let event = json!({"outcome": "failed", "evidence": "/tmp/ci/status.json"});
    assert!(pr::repair_intent(&mut t, &action, &event, 200.0).is_err());
}
