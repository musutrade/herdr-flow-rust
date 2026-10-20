//! Behavioral coverage for requirement revisions and independent activities.
pub mod common;
use herdr_flow::{
    model::J,
    progress::{
        self,
        artifact::{ArtifactAdapter, LocalArtifacts},
        lineage,
    },
    util,
};
use serde_json::json;
const SESSION: &str = "11111111-1111-1111-1111-111111111111";

fn plan() -> J {
    json!({"kind":"plan", "task":"project", "goal":"deliver a correct document",
        "basis":{"original":["user request"], "changes":[], "specifications":["style guide"],
        "endpoint":"customer/document", "confirmation":"confirmed by user"},
        "authorization":[{"id":"develop", "action":"development", "source":"user request"}]})
}
fn state() -> J {
    let mut s = json!({"id":"queue", "stopped":false, "tasks":{}, "events":{}, "waits":{}});
    lineage::action(&mut s, &plan(), 1.0).unwrap();
    s
}
fn mutate(s: &mut J, mut spec: J) -> herdr_flow::Result<J> {
    if spec.get("task").is_none() {
        spec["task"] = json!("project");
    }
    let name = spec["task"].as_str().unwrap().to_string();
    spec["state_version"] = s["tasks"][name]["state_version"].clone();
    lineage::action(s, &spec, 2.0)
}
fn file(root: &std::path::Path, name: &str, contents: &str, input: Option<&J>) -> J {
    let path = root.join(name);
    std::fs::write(&path, contents).unwrap();
    json!({"adapter":"file-sha256", "id":util::pyjson::sha256_hex(contents.as_bytes()),
        "location":path, "input":input})
}
fn add(s: &mut J, object: &J) {
    mutate(s, json!({"kind":"artifact", "artifact":object})).unwrap();
}
fn select(s: &mut J, object: &J) {
    mutate(s, json!({"kind":"candidate", "candidate":object["id"], "reason":"submit for independent acceptance"})).unwrap();
}
fn activity_spec(name: &str, role: &str, input: &J) -> J {
    json!({"kind":"activity", "activity":name, "role":role, "input":input["id"], "session_epoch":1,
        "result_wait":{"id":name, "epoch":1,"assignment":format!("{name}-a1"), "owner":role,
        "kind":"signal", "timeout_seconds":20,"reason":"await result", "condition":"finished",
        "producer_session":SESSION}})
}
fn activity(s: &mut J, name: &str, role: &str, input: &J) {
    mutate(s, activity_spec(name, role, input)).unwrap();
}
fn event(s: &mut J, name: &str) -> String {
    let w = s["waits"][name].clone();
    let id = format!("event-{name}-{}", s["events"].as_object().unwrap().len());
    let mut e =
        json!({"id":id, "task":w["task"], "wait":name, "acknowledged":true, "outcome":"complete"});
    progress::attach(s, &mut e, &w, 3.0).unwrap();
    s["events"][&id] = e;
    id
}
fn apply_spec(s: &J, name: &str, event: &str) -> J {
    json!({"task":name, "state_version":s["tasks"][name]["state_version"], "event":event,
        "acceptance":{"goal":"inspected evidence"}, "next_action":{"kind":"complete"}})
}

#[test]
fn development_output_does_not_replace_inflight_acceptance_and_apply_is_atomic() {
    let dir = common::tempdir().unwrap();
    let a = file(dir.path(), "a", "version A", None);
    let b = file(dir.path(), "b", "version B", Some(&a["id"]));
    let mut s = state();
    add(&mut s, &a);
    select(&mut s, &a);
    activity(&mut s, "review-a", "acceptance", &a);
    activity(&mut s, "dev-b", "development", &a);
    let e = event(&mut s, "dev-b");
    let mut spec = apply_spec(&s, "dev-b", &e);
    spec["output"] = b.clone();
    let before = s.clone();
    spec["output"]["id"] = json!("wrong digest");
    assert!(progress::apply(&mut s, &spec, 4.0).is_err());
    assert_eq!(s, before);
    spec["output"] = b.clone();
    progress::apply(&mut s, &spec, 4.0).unwrap();
    assert_eq!(s["tasks"]["project"]["candidate"], a["id"]);
    assert_eq!(
        s["tasks"]["project"]["activities"]["dev-b"]["output"],
        b["id"]
    );
    assert_eq!(s["tasks"]["review-a"]["candidate"], a["id"]);
    assert!(progress::apply(&mut s, &spec, 5.0).is_err());
    let e = event(&mut s, "review-a");
    let spec = apply_spec(&s, "review-a", &e);
    progress::apply(&mut s, &spec, 6.0).unwrap();
    assert_eq!(s["tasks"]["project"]["approved"]["candidate"], a["id"]);
    select(&mut s, &b);
    assert!(s["tasks"]["project"]["approved"].is_null());
    assert_eq!(s["tasks"]["project"]["delivery_target"], b["id"]);
}

#[test]
fn replacing_candidate_round_or_requirements_invalidates_collected_and_late_results() {
    for change in ["candidate", "round", "revision"] {
        let dir = common::tempdir().unwrap();
        let a = file(dir.path(), "a", "A", None);
        let b = file(dir.path(), "b", "B", Some(&a["id"]));
        let mut s = state();
        add(&mut s, &a);
        add(&mut s, &b);
        select(&mut s, &a);
        activity(&mut s, "review", "acceptance", &a);
        activity(&mut s, "late", "acceptance", &a);
        let e = event(&mut s, "review");
        match change {
            "candidate" => select(&mut s, &b),
            "round" => select(&mut s, &a),
            _ => {
                let mut revision = plan();
                revision["kind"] = json!("revise");
                revision["basis"]["changes"] = json!(["confirmed new endpoint"]);
                mutate(&mut s, revision).unwrap();
            }
        }
        assert!(!progress::current_event(&s, &s["events"][&e]));
        let spec = apply_spec(&s, "review", &e);
        let before = s.clone();
        assert!(progress::apply(&mut s, &spec, 30.0).is_err());
        assert_eq!(s, before);
        let late = event(&mut s, "late");
        assert_eq!(s["events"][late]["withdrawn"], true);
        progress::stalls(&mut s, 100.0).unwrap();
        assert_eq!(s["events"].as_object().unwrap().len(), 2);
    }
}

#[test]
fn resource_scopes_allow_isolation_but_reserve_overlapping_writers_even_when_stale() {
    let dir = common::tempdir().unwrap();
    let a = file(dir.path(), "a", "A", None);
    let mut s = state();
    add(&mut s, &a);
    select(&mut s, &a);
    let mut dev = activity_spec("dev", "development", &a);
    dev["workspace"] = json!(dir.path().join("dev"));
    dev["execution_evidence"] = json!("inspected writer");
    mutate(&mut s, dev).unwrap();
    let mut review = activity_spec("review", "acceptance", &a);
    review["workspace"] = json!(dir.path().join("dev/subdirectory"));
    review["execution_evidence"] = json!("inspected reviewer");
    let before = s.clone();
    assert!(mutate(&mut s, review.clone()).is_err());
    assert_eq!(s, before);
    review["workspace"] = json!(dir.path().join("review"));
    mutate(&mut s, review).unwrap();
    let mut revised = plan();
    revised["kind"] = json!("revise");
    mutate(&mut s, revised).unwrap();
    assert!(!lineage::current(&s, &s["tasks"]["dev"]));
    assert!(progress::reserve_writer(
        &s,
        dir.path().join("dev").to_str().unwrap(),
        "new",
        SESSION,
        "other"
    )
    .is_err());
    let version = s["tasks"]["dev"]["state_version"].clone();
    progress::control(
        &mut s,
        Some(&json!("dev")),
        Some(&version),
        Some(&json!("cancelled")),
        Some(&json!("obsolete")),
        10.0,
    )
    .unwrap();
    let release = json!({"task":"dev", "state_version":s["tasks"]["dev"]["state_version"],"evidence":"verified process stopped"});
    progress::release(&mut s, &release, 11.0).unwrap();
    assert!(progress::reserve_writer(
        &s,
        dir.path().join("dev").to_str().unwrap(),
        "new",
        SESSION,
        "other"
    )
    .is_ok());
}

#[test]
fn derived_checks_reference_requirements_without_rewriting_completion_conditions() {
    let dir = common::tempdir().unwrap();
    let a = file(dir.path(), "a", "A", None);
    let mut s = state();
    add(&mut s, &a);
    select(&mut s, &a);
    activity(&mut s, "review", "acceptance", &a);
    let stable = s["tasks"]["review"]["acceptance"].clone();
    let check = json!({"id":"format", "criterion":"goal", "source":"style guide §2", "description":"verify layout"});
    mutate(
        &mut s,
        json!({"kind":"checks", "task":"review", "checks":[check]}),
    )
    .unwrap();
    assert_eq!(s["tasks"]["review"]["acceptance"], stable);
    mutate(
        &mut s,
        json!({"kind":"checks", "task":"review", "checks":[]}),
    )
    .unwrap();
    let e = event(&mut s, "review");
    let mut spec = apply_spec(&s, "review", &e);
    spec["acceptance"] = json!({});
    assert!(
        progress::apply(&mut s, &spec, 4.0).is_err(),
        "empty derived checks cannot approve goal"
    );
    assert!(mutate(&mut s, json!({"kind":"checks", "task":"review", "checks":[{"id":"bad","criterion":"invented","description":"x","source":"x"}]})).is_err());
}

#[test]
fn adapter_verifies_real_objects_and_registration_is_bounded_and_explicit() {
    let dir = common::tempdir().unwrap();
    let a = file(dir.path(), "a", "A", None);
    let mut s = state();
    add(&mut s, &a);
    assert!(mutate(&mut s, json!({"kind":"artifact", "artifact":a})).is_err());
    std::fs::write(dir.path().join("a"), "tampered").unwrap();
    assert!(mutate(
        &mut s,
        json!({"kind":"candidate", "candidate":a["id"], "reason":"check"})
    )
    .is_err());
    assert!(LocalArtifacts
        .verify(&json!({"adapter":"unknown"}))
        .is_err());
    assert!(LocalArtifacts
        .verify(&json!({"adapter":"git", "location":dir.path(), "id":"bad"}))
        .is_err());
    assert!(LocalArtifacts
        .verify(&json!({"adapter":"file-sha256", "location":"relative", "id":"bad"}))
        .is_err());
    assert!(LocalArtifacts
        .verify(&json!({"adapter":"file-sha256", "location":dir.path(), "id":"bad"}))
        .is_err());
}

#[test]
fn persistence_keeps_bindings_and_duplicate_activity_cannot_dispatch_twice() {
    let dir = common::tempdir().unwrap();
    let a = file(dir.path(), "a", "A", None);
    let mut s = state();
    add(&mut s, &a);
    select(&mut s, &a);
    activity(&mut s, "dev", "development", &a);
    let path = dir.path().join("queue.json");
    util::atomic_write_json(&path, &s).unwrap();
    let mut restored = util::read_json(&path).unwrap();
    assert_eq!(restored, s);
    assert!(mutate(&mut restored, activity_spec("dev", "development", &a)).is_err());
    assert_eq!(restored, s);
    let mut wait = s["waits"]["dev"].clone();
    wait["lineage"]["session_epoch"] = json!(2);
    assert!(!progress::same(
        &s["tasks"]["dev"],
        &progress::binding(&wait).unwrap()
    ));
    let mut bad = plan();
    bad["kind"] = json!("revise");
    bad["state_version"] = json!(1);
    assert!(lineage::action(&mut s, &bad, 8.0).is_err());
}

#[test]
fn legacy_migration_requires_quiescence_and_preserves_no_approval() {
    let mut s = state();
    let legacy = json!({"task":"legacy", "epoch":7, "assignment":"old", "candidate":"a".repeat(40),
        "state_version":8, "phase":"blocked", "control":"running", "writer":null,
        "pending":[], "operations":{}, "history":[], "acceptance":{"old":{"evidence":"old result"}}});
    s["tasks"]["legacy"] = legacy.clone();
    let mut spec = plan();
    spec["kind"] = json!("migrate");
    spec["task"] = json!("legacy");
    for (field, value) in [
        ("writer", json!({"workspace":"/reserved"})),
        ("pending", json!(["unapplied"])),
        ("operations", json!({"send":{"phase":"sending"}})),
        ("phase", json!("executing")),
        ("pr", json!({"phase":"open"})),
    ] {
        s["tasks"]["legacy"] = legacy.clone();
        s["tasks"]["legacy"][field] = value;
        let before = s.clone();
        assert!(mutate(&mut s, spec.clone()).is_err());
        assert_eq!(s, before);
    }
    s["tasks"]["legacy"] = legacy.clone();
    let migrated = mutate(&mut s, spec.clone()).unwrap();
    assert_eq!(migrated["schema_version"], 2);
    assert_eq!(migrated["state_version"], 9);
    assert_eq!(migrated["epoch"], 7);
    assert_eq!(migrated["legacy"], legacy);
    assert!(migrated["approved"].is_null());
    assert!(mutate(&mut s, spec).is_err());
    s["tasks"]["legacy"]["schema_version"] = json!(99);
    assert!(progress::task(&s, Some(&json!("legacy")), Some(&json!(9))).is_err());
}

#[test]
fn successor_session_wait_ack_takeover_and_uncertain_send_survive_restart() {
    let dir = common::tempdir().unwrap();
    let a = file(dir.path(), "a", "A", None);
    let mut s = state();
    add(&mut s, &a);
    activity(&mut s, "dev", "development", &a);
    let e = event(&mut s, "dev");
    let mut w = s["waits"]["dev"].clone();
    w["id"] = json!("second");
    w["assignment"] = json!("dev-a2");
    w["lineage"]["session_epoch"] = json!(2);
    s["waits"]["second"] = w;
    let mut spec = apply_spec(&s, "dev", &e);
    spec["next_action"] = json!({"kind":"dispatch", "operation":"op2", "assignment":"dev-a2",
        "session":SESSION,"owner":"dev","writes":false,"authorization":"develop",
        "action":"development","instruction":"continue development","wait":"second","accept_timeout_seconds":10});
    progress::apply(&mut s, &spec, 4.0).unwrap();
    assert_eq!(s["tasks"]["dev"]["lineage"]["session_epoch"], 2);
    let v = s["tasks"]["dev"]["state_version"].clone();
    progress::begin(
        &mut s,
        Some(&json!("dev")),
        Some(&v),
        Some(&json!("op2")),
        5.0,
    )
    .unwrap();
    let mut restored: J = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
    let v = restored["tasks"]["dev"]["state_version"].clone();
    assert!(progress::begin(
        &mut restored,
        Some(&json!("dev")),
        Some(&v),
        Some(&json!("op2")),
        6.0
    )
    .is_err());
    let late_old = event(&mut restored, "dev");
    assert_eq!(restored["events"][late_old]["withdrawn"], true);
    progress::stalls(&mut restored, 20.0).unwrap();
    let alarm = restored["events"]
        .as_object()
        .unwrap()
        .values()
        .find(|e| e["kind"] == "progress_stall")
        .unwrap();
    assert!(progress::current_event(&restored, alarm));
    let accept = json!({"task":"dev","state_version":v,"operation":"op2","assignment":"dev-a2","evidence":"native session accepted"});
    assert!(progress::accept(&mut restored, &accept, "wrong-session", 21.0).is_err());
    progress::accept(&mut restored, &accept, SESSION, 21.0).unwrap();
    let e = event(&mut restored, "second");
    let mut spec = apply_spec(&restored, "dev", &e);
    spec["next_action"] = json!({"kind":"blocked","owner":"coordinator","reason":"inspected blocker","condition":"fresh dependency"});
    progress::apply(&mut restored, &spec, 22.0).unwrap();
    let mut wait = restored["waits"]["second"].clone();
    wait["id"] = json!("recovery");
    restored["waits"]["recovery"] = wait;
    let recovery = json!({"task":"dev","state_version":restored["tasks"]["dev"]["state_version"],"wait":"recovery","evidence":"inspected recovery"});
    progress::resume(&mut restored, &recovery, 23.0).unwrap();
    let mut revision = plan();
    revision["kind"] = json!("revise");
    mutate(&mut restored, revision).unwrap();
    assert!(progress::accept(&mut restored, &accept, SESSION, 24.0).is_err());
    assert!(progress::resume(&mut restored, &recovery, 24.0).is_err());
}

#[test]
fn git_adapter_rejects_missing_objects_and_blobs_and_accepts_exact_commit() {
    use std::process::Command;
    let dir = common::tempdir().unwrap();
    let git = |args: &[&str]| {
        let o = Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(args)
            .output()
            .unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        String::from_utf8(o.stdout).unwrap().trim().to_string()
    };
    git(&["init", "-q"]);
    git(&["config", "user.name", "Test"]);
    git(&["config", "user.email", "test@example.invalid"]);
    std::fs::write(dir.path().join("file"), "content").unwrap();
    git(&["add", "file"]);
    git(&["commit", "-qm", "initial"]);
    let head = git(&["rev-parse", "HEAD"]);
    let blob = git(&["rev-parse", "HEAD:file"]);
    let mut object = json!({"adapter":"git","location":dir.path(),"id":head});
    LocalArtifacts.verify(&object).unwrap();
    object["id"] = json!(blob);
    assert!(LocalArtifacts.verify(&object).is_err());
    object["id"] = json!("a".repeat(40));
    assert!(LocalArtifacts.verify(&object).is_err());
}

#[test]
fn obsolete_operation_cannot_begin_or_take_over_and_forged_links_are_rejected() {
    let dir = common::tempdir().unwrap();
    let a = file(dir.path(), "a", "A", None);
    let mut s = state();
    add(&mut s, &a);
    select(&mut s, &a);
    activity(&mut s, "review", "acceptance", &a);
    let mut forged = s["waits"]["review"].clone();
    forged["id"] = json!("forged");
    forged["task"] = json!("forged");
    s["waits"]["forged"] = forged;
    let mut spec = plan();
    spec["task"] = json!("forged");
    spec["wait"] = json!("forged");
    spec["acceptance"] = json!([{"id":"goal","description":"goal"}]);
    assert!(progress::register(&mut s, &spec, 3.0).is_err());
    let e = event(&mut s, "review");
    let mut spec = apply_spec(&s, "review", &e);
    let mut wait = s["waits"]["review"].clone();
    wait["id"] = json!("new-session");
    wait["assignment"] = json!("a2");
    wait["lineage"]["session_epoch"] = json!(2);
    s["waits"]["new-session"] = wait;
    spec["next_action"] = json!({"kind":"dispatch","operation":"op","assignment":"a2","session":SESSION,"owner":"review",
        "writes":false,"authorization":"develop","action":"development","instruction":"check","wait":"new-session"});
    progress::apply(&mut s, &spec, 4.0).unwrap();
    select(&mut s, &a);
    let v = s["tasks"]["review"]["state_version"].clone();
    assert!(progress::begin(
        &mut s,
        Some(&json!("review")),
        Some(&v),
        Some(&json!("op")),
        5.0
    )
    .unwrap_err()
    .message
    .contains("obsolete"));
    let accept = json!({"task":"review","state_version":v,"operation":"op","assignment":"a2","evidence":"accepted"});
    assert!(progress::accept(&mut s, &accept, SESSION, 6.0)
        .unwrap_err()
        .message
        .contains("obsolete"));
}

#[test]
fn pause_keeps_arriving_results_and_resume_does_not_require_reexecution() {
    let dir = common::tempdir().unwrap();
    let a = file(dir.path(), "a", "A", None);
    let mut s = state();
    add(&mut s, &a);
    select(&mut s, &a);
    activity(&mut s, "review", "acceptance", &a);
    let v = s["tasks"]["project"]["state_version"].clone();
    progress::control(
        &mut s,
        Some(&json!("project")),
        Some(&v),
        Some(&json!("paused")),
        Some(&json!("user pause")),
        3.0,
    )
    .unwrap();
    let e = event(&mut s, "review");
    assert!(s["events"][&e].get("withdrawn").is_none());
    assert!(!progress::current_event(&s, &s["events"][&e]));
    let spec = apply_spec(&s, "review", &e);
    assert!(progress::apply(&mut s, &spec, 4.0).is_err());
    assert_eq!(
        progress::diagnose(&s, &s["tasks"]["review"], Some(4.0)).unwrap()["classification"],
        "parent_inactive"
    );
    progress::stalls(&mut s, 100.0).unwrap();
    assert_eq!(s["events"].as_object().unwrap().len(), 1);
    let v = s["tasks"]["project"]["state_version"].clone();
    progress::control(
        &mut s,
        Some(&json!("project")),
        Some(&v),
        Some(&json!("running")),
        Some(&json!("resume")),
        5.0,
    )
    .unwrap();
    assert_eq!(
        progress::diagnose(&s, &s["tasks"]["project"], Some(5.0)).unwrap()["classification"],
        "coordinating"
    );
    assert!(progress::current_event(&s, &s["events"][&e]));
    progress::apply(&mut s, &spec, 6.0).unwrap();
    select(&mut s, &a);
    assert_eq!(
        progress::diagnose(&s, &s["tasks"]["review"], Some(7.0)).unwrap()["classification"],
        "obsolete_activity"
    );
}

#[test]
fn customer_adapter_owns_verification_through_registration_and_application() {
    struct DocumentAdapter;
    impl ArtifactAdapter for DocumentAdapter {
        fn verify(&self, artifact: &J) -> herdr_flow::Result<()> {
            assert_eq!(artifact["adapter"], "document");
            let mut local = artifact.clone();
            local["adapter"] = json!("file-sha256");
            LocalArtifacts.verify(&local)
        }
    }
    let dir = common::tempdir().unwrap();
    let mut a = file(dir.path(), "a", "A", None);
    a["adapter"] = json!("document");
    let mut s = state();
    let run = |s: &mut J, mut spec: J| {
        spec["task"] = json!("project");
        spec["state_version"] = s["tasks"]["project"]["state_version"].clone();
        lineage::action_with_adapter(s, &spec, 2.0, &DocumentAdapter).unwrap()
    };
    run(&mut s, json!({"kind":"artifact","artifact":a}));
    run(
        &mut s,
        json!({"kind":"candidate","candidate":a["id"],"reason":"review"}),
    );
    run(&mut s, activity_spec("review", "acceptance", &a));
    let e = event(&mut s, "review");
    let spec = apply_spec(&s, "review", &e);
    assert!(
        progress::apply(&mut s, &spec, 3.0).is_err(),
        "CLI cannot pretend to verify a custom object"
    );
    progress::apply_with_adapter(&mut s, &spec, 3.0, &DocumentAdapter).unwrap();
    assert_eq!(s["tasks"]["project"]["approved"]["candidate"], a["id"]);
}

#[test]
fn linked_activity_cannot_be_migrated_into_an_unrelated_parent() {
    let dir = common::tempdir().unwrap();
    let a = file(dir.path(), "a", "A", None);
    let mut s = state();
    add(&mut s, &a);
    activity(&mut s, "dev", "development", &a);
    let v = s["tasks"]["dev"]["state_version"].clone();
    progress::control(
        &mut s,
        Some(&json!("dev")),
        Some(&v),
        Some(&json!("cancelled")),
        Some(&json!("cancel activity")),
        3.0,
    )
    .unwrap();
    let mut spec = plan();
    spec["kind"] = json!("migrate");
    spec["task"] = json!("dev");
    let before = s.clone();
    assert!(mutate(&mut s, spec).is_err());
    assert_eq!(s, before);
}

#[test]
fn opencode_successor_preserves_adapter_through_writer_and_takeover() {
    let dir = common::tempdir().unwrap();
    let a = file(dir.path(), "a", "A", None);
    let mut s = state();
    add(&mut s, &a);
    activity(&mut s, "dev", "development", &a);
    let e = event(&mut s, "dev");
    let mut w = s["waits"]["dev"].clone();
    w["id"] = json!("second");
    w["assignment"] = json!("dev-a2");
    w["lineage"]["session_epoch"] = json!(2);
    w.as_object_mut().unwrap().remove("producer_session");
    w["producer"] = json!({"adapter":"opencode","session":"sesnew"});
    s["waits"]["second"] = w;
    let mut spec = apply_spec(&s, "dev", &e);
    spec["next_action"] = json!({"kind":"dispatch","operation":"op2","assignment":"dev-a2","adapter":"opencode","session":"sesnew",
        "owner":"developer","writes":true,"workspace":dir.path(),"authorization":"develop","action":"development","instruction":"continue","wait":"second"});
    let mut wrong = spec.clone();
    wrong["next_action"]["adapter"] = json!("codex");
    assert!(progress::apply(&mut s, &wrong, 4.0).is_err());
    progress::apply(&mut s, &spec, 4.0).unwrap();
    assert_eq!(
        s["tasks"]["dev"]["writer"]["producer"]["adapter"],
        "opencode"
    );
    let v = s["tasks"]["dev"]["state_version"].clone();
    progress::begin(
        &mut s,
        Some(&json!("dev")),
        Some(&v),
        Some(&json!("op2")),
        5.0,
    )
    .unwrap();
    let v = s["tasks"]["dev"]["state_version"].clone();
    progress::accept_identity(&mut s,&json!({"task":"dev","state_version":v,"operation":"op2","assignment":"dev-a2","evidence":"actual native takeover"}),&json!({"adapter":"opencode","session":"sesnew"}),6.0).unwrap();
    assert_eq!(
        s["tasks"]["dev"]["operations"]["op2"]["adapter"],
        "opencode"
    );
}
