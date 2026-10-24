//! Disposable local Git fixtures only. The preflight must never mutate them.
mod common;

use herdr_flow::{model::J, queue::cleanup_preflight, util};
use serde_json::json;
use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::{symlink, PermissionsExt},
    path::{Path, PathBuf},
    process::Command,
};

struct Fixture {
    _temp: tempfile::TempDir,
    main: PathBuf,
    checkout: PathBuf,
    queue: PathBuf,
    state: J,
    head: String,
}

fn git(p: &Path, args: &[&str]) -> String {
    let result = Command::new("git")
        .arg("-C")
        .arg(p)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    String::from_utf8(result.stdout).unwrap().trim().to_owned()
}

fn landed(head: &str, checkout: &Path) -> J {
    json!({"checkout":checkout,"phase":"merged","head":head,"number":7,"repo":"owner/repo","base_ref":"main",
        "landing":{"head":head,"commit":head,"latest_base":head,"number":7,"repo":"owner/repo","base_ref":"main","observed_at":10},
        "remote":{"merged":true,"state":"closed","head":head,"at":10},
        "publication":{"kind":"create","phase":"observed","head":head,"observed_at":10}})
}

impl Fixture {
    fn new() -> Self {
        let temp = common::tempdir().unwrap();
        let main = temp.path().join("main");
        common::create_dir(&main).unwrap();
        git(&main, &["init", "-b", "main"]);
        git(&main, &["config", "user.email", "fixture@example.invalid"]);
        git(&main, &["config", "user.name", "Fixture"]);
        fs::write(main.join("tracked"), "initial\n").unwrap();
        git(&main, &["add", "tracked"]);
        git(&main, &["commit", "-m", "initial"]);
        let head = git(&main, &["rev-parse", "HEAD"]);
        let checkout = temp.path().join("linked");
        git(
            &main,
            &[
                "worktree",
                "add",
                "--detach",
                checkout.to_str().unwrap(),
                &head,
            ],
        );
        let queue = temp.path().join("queue");
        common::create_dir(&queue).unwrap();
        let task = json!({"task":"t","epoch":1,"state_version":1,"candidate":head,"assignment":"a","schema_version":1,
            "phase":"completed","control":"running","writer":null,"pending":[],"operations":{},"findings":{},"next_action":{"kind":"complete"},"pr":landed(&head,&checkout)});
        let mut state = json!({"version":1,"id":"q","tasks":{"t":task},"waits":{},"events":{},"deliveries":{},"active":null});
        settled_wait(&mut state, "t", "w", "e");
        Self {
            _temp: temp,
            main,
            checkout,
            queue,
            state,
            head,
        }
    }
    fn save(&self) {
        util::atomic_write_json(&self.queue.join("queue.json"), &self.state).unwrap();
    }
    fn spec(&self) -> J {
        json!({"queue_id":"q","task":"t","state_version":1})
    }
    fn inspect(&self) -> J {
        self.save();
        cleanup_preflight::inspect(&self.queue, &self.spec()).unwrap()
    }
    fn code(&self, code: &str) {
        let r = self.inspect();
        assert!(has(&r, code), "{r}");
        assert_ne!(r["queue_git_status"], "clear");
    }
}

fn has(r: &J, code: &str) -> bool {
    r["blockers"]
        .as_array()
        .unwrap()
        .iter()
        .any(|b| b["code"] == code)
}

#[test]
fn clear_is_scoped_and_never_deletion_permission() {
    let x = Fixture::new();
    let r = x.inspect();
    assert_eq!(r["queue_git_status"], "clear", "{r}");
    assert_eq!(r["snapshot"], true);
    assert_eq!(r["deletion_authorized"], false);
    assert_eq!(r["owner_recheck_required"], true);
    assert_eq!(r["not_checked"].as_array().unwrap().len(), 4);
    assert_eq!(r["git"]["head"], x.head);
    assert_eq!(cleanup_preflight::exit_code(&r), 0);
    assert!(cleanup_preflight::summary(&r).contains("not checked:"));
}

#[test]
fn strict_spec_and_state_identity() {
    let x = Fixture::new();
    x.save();
    for (key, value) in [
        ("queue_id", json!("other")),
        ("task", json!("absent")),
        ("state_version", json!(2)),
        ("extra", json!(true)),
        ("checkout", json!("relative")),
    ] {
        let mut spec = x.spec();
        spec[key] = value;
        assert!(
            cleanup_preflight::inspect(&x.queue, &spec).is_err(),
            "{spec}"
        );
    }
}

#[test]
fn released_writer_evidence_does_not_invent_checkout() {
    let mut x = Fixture::new();
    x.state["tasks"]["t"]["pr"] = J::Null;
    x.state["tasks"]["t"]["last_release"] =
        json!({"workspace":x.checkout,"evidence":"/arbitrary/file"});
    x.code("checkout_selection");
}

#[test]
fn ambiguity_and_unrecorded_checkout_refuse() {
    let mut x = Fixture::new();
    x.state["tasks"]["t"]["writer"] = json!({"workspace":x.main});
    x.code("checkout_selection");
    x.save();
    let mut spec = x.spec();
    spec["checkout"] = json!(x.checkout);
    let r = cleanup_preflight::inspect(&x.queue, &spec).unwrap();
    assert!(has(&r, "writer"));
    spec["checkout"] = json!(x.queue);
    assert!(has(
        &cleanup_preflight::inspect(&x.queue, &spec).unwrap(),
        "checkout_selection"
    ));
}

#[test]
fn aliases_subdirectories_and_main_checkout_refuse() {
    let mut x = Fixture::new();
    let alias = x.queue.parent().unwrap().join("alias");
    symlink(&x.checkout, &alias).unwrap();
    x.state["tasks"]["t"]["pr"]["checkout"] = json!(alias);
    x.code("checkout_selection");
    x.state["tasks"]["t"]["pr"]["checkout"] = json!(x.main);
    x.code("git_observation_failed");
    let child = x.checkout.join("child");
    common::create_dir_all(&child).unwrap();
    x.state["tasks"]["t"]["pr"]["checkout"] = json!(child);
    x.code("git_observation_failed");
}

#[test]
fn writers_pending_operations_and_phases_block_individually() {
    let mut x = Fixture::new();
    let original = x.state.clone();
    for (key, value, code) in [
        ("writer", json!({"workspace":x.checkout}), "writer"),
        ("pending", json!(["event"]), "pending_results"),
        (
            "operations",
            json!({"op":{"phase":"sending"}}),
            "unfinished_operation",
        ),
        ("phase", json!("next_pending"), "task_not_settled"),
        ("phase", json!("future_unknown"), "task_phase_unknown"),
    ] {
        x.state = original.clone();
        x.state["tasks"]["t"][key] = value;
        x.code(code);
    }
}

#[test]
fn foreign_writer_survives_cancel_and_completion() {
    let mut x = Fixture::new();
    let mut foreign = x.state["tasks"]["t"].clone();
    foreign["task"] = json!("other");
    foreign["pr"] = J::Null;
    foreign["control"] = json!("cancelled");
    foreign["writer"] = json!({"workspace":x.checkout});
    x.state["tasks"]["other"] = foreign;
    settled_wait(&mut x.state, "other", "w-other", "e-other");
    x.code("writer");
    x.state["tasks"]["other"]["writer"] = json!({"workspace":27});
    x.code("queue_reference_invalid");
}

fn add_result(x: &mut Fixture) {
    x.state["waits"]["w"] = json!({"task":"t","epoch":1,"candidate":x.head,"assignment":"a","outcome":"complete","closed":false});
    x.state["events"]["e"] = json!({"task":"t","wait":"w","binding":{"task":"t","epoch":1,"candidate":x.head,"assignment":"a"},
        "outcome":"complete","acknowledged":true,"applied":true});
    x.state["deliveries"]["d"] = json!({"phase":"acknowledged","events":["e"]});
    x.state["tasks"]["t"]["operations"]["op"] =
        json!({"phase":"accepted","assignment":"a","intent":{"wait":"w","workspace":x.checkout}});
}

#[test]
fn completed_applied_waits_are_historical_even_if_not_closed() {
    let mut x = Fixture::new();
    add_result(&mut x);
    let r = x.inspect();
    assert_eq!(r["queue_git_status"], "clear", "{r}");
    assert!(r["historical_references"]
        .as_array()
        .unwrap()
        .iter()
        .any(|h| h["kind"] == "completed_wait"));
}

#[test]
fn result_operation_binding_and_live_wait_delivery_fail_closed() {
    let mut x = Fixture::new();
    add_result(&mut x);
    let original = x.state.clone();
    for (pointer, value, code) in [
        ("/events/e/applied", json!(false), "unsettled_result"),
        ("/events/e/outcome", json!("failed"), "unfinished_operation"),
        (
            "/events/e/binding/candidate",
            json!("f".repeat(40)),
            "unfinished_operation",
        ),
        ("/waits/w/outcome", json!("pending"), "unresolved_wait"),
        (
            "/deliveries/d/phase",
            json!("uncertain"),
            "active_or_unresolved_delivery",
        ),
        ("/active", json!("d"), "active_or_unresolved_delivery"),
    ] {
        x.state = original.clone();
        *x.state.pointer_mut(pointer).unwrap() = value;
        x.code(code);
    }
}

#[test]
fn all_unlanded_publications_block_and_inconsistent_landing_does_not_release() {
    let mut x = Fixture::new();
    for phase in [
        "publication_prepared",
        "publication_uncertain",
        "open",
        "closed_unmerged",
        "future",
    ] {
        x.state["tasks"]["t"]["pr"]["phase"] = json!(phase);
        x.code("unresolved_publication");
    }
    x.state["tasks"]["t"]["pr"]["phase"] = json!("merged");
    x.state["tasks"]["t"]["pr"]["remote"]["head"] = json!("f".repeat(40));
    x.code("unresolved_publication");
}

#[test]
fn dirt_locks_head_and_retention() {
    let mut x = Fixture::new();
    fs::write(x.checkout.join("new"), "untracked").unwrap();
    x.code("dirty_checkout");
    fs::remove_file(x.checkout.join("new")).unwrap();
    fs::write(x.checkout.join("tracked"), "changed").unwrap();
    x.code("dirty_checkout");
    git(&x.checkout, &["add", "tracked"]);
    git(&x.checkout, &["commit", "-m", "detached output"]);
    x.head = git(&x.checkout, &["rev-parse", "HEAD"]);
    x.state["tasks"]["t"]["candidate"] = json!(x.head);
    x.state["tasks"]["t"]["pr"] = landed(&x.head, &x.checkout);
    settled_wait(&mut x.state, "t", "w", "e");
    x.code("missing_durable_ref");
    git(&x.checkout, &["update-ref", "refs/archive/output", &x.head]);
    assert_eq!(x.inspect()["queue_git_status"], "clear");
    let dir = git(&x.checkout, &["rev-parse", "--absolute-git-dir"]);
    fs::write(Path::new(&dir).join("index.lock"), "").unwrap();
    x.code("git_locks");
}

#[test]
fn shared_boundaries_and_partial_clone_refuse() {
    let mut x = Fixture::new();
    x.state["guard_dir"] = json!(x.checkout.join("guard"));
    x.code("shared_boundary_overlap");
    x.state["guard_dir"] = J::Null;
    x.state["receiver"] = json!({"cwd":x.checkout});
    x.code("shared_boundary_overlap");
    x.state["receiver"] = J::Null;
    git(&x.main, &["config", "remote.origin.promisor", "true"]);
    x.code("git_observation_failed");
}

fn snapshot(root: &Path) -> BTreeMap<PathBuf, (u32, Vec<u8>)> {
    fn walk(root: &Path, out: &mut BTreeMap<PathBuf, (u32, Vec<u8>)>) {
        let meta = fs::symlink_metadata(root).unwrap();
        let bytes = if meta.is_file() {
            fs::read(root).unwrap()
        } else {
            Vec::new()
        };
        out.insert(root.to_owned(), (meta.permissions().mode(), bytes));
        if meta.is_dir() {
            for e in fs::read_dir(root).unwrap() {
                walk(&e.unwrap().path(), out);
            }
        }
    }
    let mut result = BTreeMap::new();
    walk(root, &mut result);
    result
}

#[test]
fn cli_does_not_execute_fsmonitor_or_change_queue_index_refs_or_entries() {
    let x = Fixture::new();
    x.save();
    let root = x.queue.parent().unwrap();
    let hook = root.join("monitor");
    let marker = root.join("EXECUTED");
    fs::write(&hook, format!("#!/bin/sh\ntouch '{}'\n", marker.display())).unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o700)).unwrap();
    git(
        &x.main,
        &["config", "core.fsmonitor", hook.to_str().unwrap()],
    );
    let spec = root.join("spec.json");
    fs::write(&spec, serde_json::to_vec(&x.spec()).unwrap()).unwrap();
    let before = snapshot(root);
    let output = Command::new(env!("CARGO_BIN_EXE_herdr-flow"))
        .args(["queue", "cleanup-preflight", "--state-dir"])
        .arg(&x.queue)
        .arg("--spec")
        .arg(&spec)
        .arg("--json")
        .env("GIT_OPTIONAL_LOCKS", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!marker.exists());
    assert_eq!(before, snapshot(root));
    assert!(!x.queue.join("queue.lock").exists());
}

#[test]
fn absent_state_cli_creates_nothing_and_unsafe_state_refuses() {
    let x = Fixture::new();
    let root = x.queue.parent().unwrap();
    let missing = root.join("missing");
    let spec = root.join("spec.json");
    fs::write(&spec, serde_json::to_vec(&x.spec()).unwrap()).unwrap();
    let before = snapshot(root);
    let output = Command::new(env!("CARGO_BIN_EXE_herdr-flow"))
        .args(["queue", "cleanup-preflight", "--state-dir"])
        .arg(&missing)
        .arg("--spec")
        .arg(&spec)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert_eq!(before, snapshot(root));
    assert!(!missing.exists());
    x.save();
    fs::set_permissions(&x.queue, fs::Permissions::from_mode(0o777)).unwrap();
    assert!(cleanup_preflight::inspect(&x.queue, &x.spec()).is_err());
}

fn lane_fixture(x: &mut Fixture) {
    let pr = landed(&x.head, &x.checkout);
    let member = json!({"task":"t","epoch":1,"head":x.head,"checkout":x.checkout,"receipt_id":"receipt",
        "own_commits":[x.head],"host":"github.com","repo":"owner/repo","original_base":x.head});
    let batch = json!({"id":"batch","phase":"landed","members":[member],"pr":pr,"landing":pr["landing"],"snapshot":{"head":x.head,"tree":x.head}});
    let mut lane = x.state["tasks"]["t"].clone();
    lane["task"] = json!("lane");
    lane["pr"] = J::Null;
    lane["lane"] = json!({"pending":[],"active":null,"history":[batch],"capacity":1,"host":"github.com","repo":"owner/repo","base_ref":"main"});
    x.state["tasks"]["lane"] = lane;
    let mut landing = pr["landing"].clone();
    landing["batch"] = json!("batch");
    landing["batch_head"] = json!(x.head);
    landing["tree"] = json!(x.head);
    x.state["tasks"]["t"]["batch_delivery"] = json!({"owner":"lane","phase":"landed","head":x.head,"own_commits":[x.head],"receipt_id":"receipt","batch":"batch","landing":landing});
}

#[test]
fn lane_history_is_typed_and_active_pending_or_unverified_history_blocks() {
    let mut x = Fixture::new();
    lane_fixture(&mut x);
    let original = x.state.clone();
    assert_eq!(x.inspect()["queue_git_status"], "clear");
    let batch = x.state["tasks"]["lane"]["lane"]["history"][0].clone();
    x.state["tasks"]["t"]["pr"] = J::Null; // Path remains mapped through the exact source member.
    assert_eq!(x.inspect()["queue_git_status"], "clear");
    x.state["tasks"]["lane"]["lane"]["pending"] = batch["members"].clone();
    x.code("pending_lane_member");
    x.state = original.clone();
    x.state["tasks"]["lane"]["lane"]["active"] = batch.clone();
    x.code("active_batch");
    x.state = original.clone();
    x.state["tasks"]["lane"]["lane"]["history"][0]["phase"] = json!("cancelled");
    x.code("unverified_batch_history");
    x.state = original.clone();
    x.state["tasks"]["lane"]["lane"]["history"][0]["members"][0]["epoch"] = json!(2);
    x.code("checkout_provenance");
    x.state = original;
    x.state["tasks"]["t"]["batch_delivery"]["landing"]["head"] = json!("f".repeat(40));
    x.code("queue_reference_invalid");
}

fn successor_fixture(x: &mut Fixture) {
    lane_fixture(x);
    let root = json!({"version":1,"kind":"root","lane":"lane","previous":null,"admission_evidence":"/fixture/admission"});
    x.state["tasks"]["t"]["schema_version"] = json!(3);
    x.state["tasks"]["t"]["claim"] = root.clone();
    x.state["tasks"]["t"]["batch_delivery"]["claimed_by"] = json!({"task":"next","epoch":1});
    let mut next = x.state["tasks"]["t"].clone();
    next["task"] = json!("next");
    next["pr"] = J::Null;
    next["batch_delivery"] = J::Null;
    next["claim"]["previous"] =
        json!({"task":"t","epoch":1,"candidate":x.head,"receipt_id":"receipt"});
    x.state["tasks"]["next"] = next;
    settled_wait(&mut x.state, "next", "w-next", "e-next");
}

#[test]
fn claimed_successor_is_historical_only_after_exact_chain_and_settled_work() {
    let mut x = Fixture::new();
    successor_fixture(&mut x);
    let original = x.state.clone();
    let r = x.inspect();
    assert_eq!(r["queue_git_status"], "clear", "{r}");
    assert!(r["historical_references"]
        .as_array()
        .unwrap()
        .iter()
        .any(|v| v["kind"] == "settled_claim_marker"));
    for (pointer, value, code) in [
        ("/tasks/next/epoch", json!(2), "queue_reference_invalid"),
        (
            "/tasks/next/claim/previous/receipt_id",
            json!("wrong"),
            "queue_reference_invalid",
        ),
        ("/tasks/next/phase", json!("waiting"), "task_not_settled"),
        ("/tasks/next/pending", json!(["late"]), "pending_results"),
        ("/tasks/next/writer", json!({"workspace":x.main}), "writer"),
        ("/tasks/next", J::Null, "queue_reference_invalid"),
    ] {
        x.state = original.clone();
        *x.state.pointer_mut(pointer).unwrap() = value;
        x.code(code);
        assert!(!x.inspect()["historical_references"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["kind"] == "settled_claim_marker"));
    }
}

#[test]
fn related_member_work_blocks_and_missing_active_delivery_is_unknown() {
    let mut x = Fixture::new();
    successor_fixture(&mut x);
    let mut child = x.state["tasks"]["next"].clone();
    child["task"] = json!("child");
    child["claim"] = json!({"version":1,"kind":"member","root":"next","root_epoch":1,"admission_evidence":"/fixture/admission"});
    child["pending"] = json!(["late"]);
    x.state["tasks"]["child"] = child;
    settled_wait(&mut x.state, "child", "w-child", "e-child");
    x.code("pending_results");
    x.state["tasks"]["child"]["pending"] = json!([]);
    assert_eq!(x.inspect()["queue_git_status"], "clear");
    x.state["active"] = json!("missing");
    x.code("queue_reference_invalid");
}

fn settled_wait(s: &mut J, name: &str, wait: &str, event: &str) {
    s["tasks"][name]["wait"] = json!(wait);
    let t = &s["tasks"][name];
    let binding = json!({"task":name,"epoch":t["epoch"],"candidate":t["candidate"],"assignment":t["assignment"]});
    let mut w = binding.clone();
    w["outcome"] = json!("complete");
    w["closed"] = json!(false);
    s["waits"][wait] = w;
    s["events"][event] = json!({"task":name,"wait":wait,"binding":binding,"outcome":"complete","acknowledged":true,"applied":true});
}

#[test]
fn findings_current_wait_and_task_candidate_are_verified() {
    let mut x = Fixture::new();
    let original = x.state.clone();
    for (key, value, code) in [
        (
            "findings",
            json!({"f":{"blocking":true,"resolved":null}}),
            "unresolved_blocking_finding",
        ),
        (
            "findings",
            json!({"f":{"blocking":"unknown"}}),
            "queue_reference_invalid",
        ),
        ("wait", json!("absent"), "queue_reference_invalid"),
        ("wait", J::Null, "queue_reference_invalid"),
        (
            "next_action",
            json!({"kind":"dispatch","operation":"missing"}),
            "queue_reference_invalid",
        ),
        (
            "next_action",
            json!({"kind":"unknown"}),
            "queue_reference_invalid",
        ),
        ("candidate", json!("f".repeat(40)), "checkout_provenance"),
    ] {
        x.state = original.clone();
        x.state["tasks"]["t"][key] = value;
        x.code(code);
    }
    x.state = original;
    x.state["tasks"]["t"]["findings"] =
        json!({"f":{"blocking":true,"resolved":"/fixture/resolution"}});
    assert_eq!(x.inspect()["queue_git_status"], "clear");
}

#[test]
fn detached_descendant_and_reflog_do_not_replace_durable_ref() {
    let mut x = Fixture::new();
    fs::write(x.checkout.join("tracked"), "unique output").unwrap();
    git(&x.checkout, &["add", "tracked"]);
    git(&x.checkout, &["commit", "-m", "unreferenced output"]);
    x.head = git(&x.checkout, &["rev-parse", "HEAD"]);
    x.state["tasks"]["t"]["candidate"] = json!(x.head);
    x.state["tasks"]["t"]["pr"] = landed(&x.head, &x.checkout);
    settled_wait(&mut x.state, "t", "w", "e");
    let descendant = x.queue.parent().unwrap().join("descendant");
    git(
        &x.main,
        &[
            "worktree",
            "add",
            "--detach",
            descendant.to_str().unwrap(),
            &x.head,
        ],
    );
    git(
        &descendant,
        &["commit", "--allow-empty", "-m", "detached descendant"],
    );
    x.code("missing_durable_ref");
    git(
        &descendant,
        &["update-ref", "refs/archive/descendant", "HEAD"],
    );
    assert_eq!(x.inspect()["queue_git_status"], "clear");
}

#[test]
fn dispatch_next_action_requires_saved_operation_and_exact_applied_result() {
    let mut x = Fixture::new();
    add_result(&mut x);
    x.state["tasks"]["t"]["next_action"] = json!({"kind":"dispatch","operation":"op"});
    assert_eq!(x.inspect()["queue_git_status"], "clear");
    x.state["tasks"]["t"]["operations"]["op"]["phase"] = json!("sending");
    x.code("unfinished_operation");
}

#[test]
fn mismatched_candidate_is_refused_before_selection_even_with_another_record() {
    let mut x = Fixture::new();
    x.state["tasks"]["t"]["writer"] = json!({"workspace":x.checkout});
    x.state["tasks"]["t"]["candidate"] = json!("f".repeat(40));
    let r = x.inspect();
    assert!(has(&r, "checkout_provenance"));
    assert!(r["candidate_checkout"].is_null());
    assert_eq!(r["queue_git_status"], "unknown");
}

#[test]
fn recorded_ci_hold_is_not_released_by_a_merged_boolean() {
    let mut x = Fixture::new();
    for state in ["pending", "failure", "needs_attention", "unknown"] {
        x.state["tasks"]["t"]["pr"]["observation"] = json!({"state":state});
        x.code("unresolved_publication");
    }
    x.state["tasks"]["t"]["pr"]["observation"] = json!({"state":"success","failures":[{}]});
    x.code("unresolved_publication");
    x.state["tasks"]["t"]["pr"]["observation"] = json!({"state":"success","failures":[]});
    assert_eq!(x.inspect()["queue_git_status"], "clear");
    lane_fixture(&mut x);
    x.state["tasks"]["lane"]["lane"]["history"][0]["ci"] = json!({"state":"pending"});
    x.code("unverified_batch_history");
    x.state["tasks"]["lane"]["lane"]["history"][0]["ci"] = json!({"state":"success"});
    assert_eq!(x.inspect()["queue_git_status"], "clear");
}

#[test]
fn history_pr_and_landing_must_match_the_frozen_snapshot_head() {
    let mut x = Fixture::new();
    lane_fixture(&mut x);
    let b = &mut x.state["tasks"]["lane"]["lane"]["history"][0];
    for pointer in [
        "/pr/head",
        "/pr/landing/head",
        "/pr/remote/head",
        "/pr/publication/head",
        "/landing/head",
    ] {
        *b.pointer_mut(pointer).unwrap() = json!("f".repeat(40));
    }
    x.code("unverified_batch_history");
}
