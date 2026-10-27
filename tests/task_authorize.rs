//! No-model authorization repair; synthetic queue identities only.
pub mod common;
use herdr_flow::{model::J, pr, progress::authorize, queue, util};
use serde_json::json;

const SESSION: &str = "11111111-1111-1111-1111-111111111111";

fn state(schema: i64) -> J {
    let mut t = json!({"task":"lane","epoch":1,"schema_version":schema,"state_version":12,
        "phase":"blocked","control":"running","writer":null,"pending":[],"operations":{},
        "history":[],"authorization":{"review":{"action":"verification","source":"original user request"}},
        "claim":{"version":1,"kind":"independent","admission_evidence":"/evidence/decision"},
        "lane":{"active":{"id":"A","phase":"snapshot_frozen"},"pending":[{"task":"B","receipt_id":"receipt-B"}]},
        "pr":{"phase":"publication_prepared","number":null,"observation":null,"landing":null,
            "publication":{"kind":"create","phase":"prepared"}}});
    if schema != 3 {
        t.as_object_mut().unwrap().remove("claim");
    }
    json!({"stopped":false,"events":{},"tasks":{"lane":t,
        "A":{"batch_delivery":{"receipt_id":"receipt-A","claimed_by":{"task":"B","epoch":1}}},
        "B":{"claim":{"previous":{"task":"A","receipt_id":"receipt-A"}}}},
        "waits":{"final":{"outcome":"pending"}},"active":null})
}
fn spec() -> J {
    json!({"task":"lane","state_version":12,"id":"publish","action":"publication",
        "source":"Existing user approval: /evidence/live-authorization.json, exact A/B PRs and required CI"})
}
fn refusal(s: &mut J, input: &J) {
    let before = s.clone();
    assert!(authorize::append(s, input, 100.0).is_err());
    assert_eq!(*s, before);
}

#[test]
fn adds_only_audited_authorization_preserving_lane_and_claims() {
    for schema in [1, 3] {
        let mut s = state(schema);
        let mut expected = s.clone();
        let result = authorize::append(&mut s, &spec(), 100.0).unwrap();
        expected["tasks"]["lane"]["authorization"]["publish"] =
            json!({"action":"publication","source":spec()["source"]});
        expected["tasks"]["lane"]["state_version"] = json!(13);
        expected["tasks"]["lane"]["history"] = json!([{"at":100.0,"version":13,
            "action":"authorization_added","id":"publish","authorization":{
                "action":"publication","source":spec()["source"]}}]);
        assert_eq!(s, expected);
        assert_eq!(result, s["tasks"]["lane"]);
        refusal(&mut s, &spec()); // Original version cannot be replayed.
        let before = s.clone();
        let mut retry = spec();
        retry["state_version"] = json!(13);
        authorize::append(&mut s, &retry, 101.0).unwrap();
        assert_eq!(s, before); // Current-version identical entry is a no-op.
        retry["source"] = json!("different authorization");
        refusal(&mut s, &retry);
    }
}

#[test]
fn rejects_invalid_input_without_partial_mutation() {
    for (key, value) in [
        ("id", json!("")),
        ("id", json!("x".repeat(65))),
        ("id", json!(1)),
        ("action", json!("development")),
        ("source", json!("")),
        ("source", json!(null)),
        ("source", json!("x".repeat(1001))),
        ("state_version", json!(11)),
        ("state_version", json!("12")),
        ("task", json!("missing")),
        ("overwrite", json!(true)),
    ] {
        let mut input = spec();
        input[key] = value;
        refusal(&mut state(3), &input);
    }
    let mut missing = spec();
    missing.as_object_mut().unwrap().remove("source");
    refusal(&mut state(3), &missing);
    for schema in [2, 0, 4] {
        refusal(&mut state(schema), &spec());
    }
    let mut legacy = state(1);
    legacy["tasks"]["lane"]
        .as_object_mut()
        .unwrap()
        .remove("schema_version");
    refusal(&mut legacy, &spec());
}

#[test]
fn rejects_non_quiescent_or_owned_state() {
    for (pointer, value) in [
        ("/stopped", json!(true)),
        ("/stopped", json!(null)),
        ("/tasks/lane/control", json!("paused")),
        ("/tasks/lane/control", json!("cancelled")),
        ("/tasks/lane/phase", json!("waiting")),
        ("/tasks/lane/phase", json!("next_pending")),
        ("/tasks/lane/phase", json!("completed")),
        ("/tasks/lane/writer", json!({})),
        ("/tasks/lane/pending", json!(["late"])),
        ("/tasks/lane/pending", json!(null)),
        ("/tasks/lane/operations", json!({"op":{"phase":"sending"}})),
        ("/tasks/lane/operations", json!({"op":{"phase":"prepared"}})),
        (
            "/tasks/lane/operations",
            json!({"op":{"phase":"uncertain"}}),
        ),
        ("/tasks/lane/operations", json!(null)),
        ("/tasks/lane/history", json!(null)),
        ("/tasks/lane/authorization", json!(null)),
        ("/tasks/lane/task", json!("other")),
        ("/tasks/lane/lane/active/phase", json!("ci_passed")),
        ("/tasks/lane/lane/active", json!(null)),
        ("/tasks/lane/pr/publication/phase", json!("sending")),
        ("/tasks/lane/pr/publication/phase", json!("uncertain")),
        ("/tasks/lane/pr/publication/kind", json!("update")),
        ("/tasks/lane/pr/phase", json!("observed")),
        ("/tasks/lane/pr/number", json!(7)),
        ("/tasks/lane/pr/observation", json!({})),
        ("/tasks/lane/pr/landing", json!({})),
    ] {
        let mut s = state(3);
        *s.pointer_mut(pointer).unwrap() = value;
        refusal(&mut s, &spec());
    }
    for key in ["delivery", "batch_delivery", "lineage"] {
        let mut s = state(3);
        s["tasks"]["lane"][key] = json!({"claimed_by":{"task":"successor"}});
        refusal(&mut s, &spec());
    }
}

#[test]
fn accepted_operations_need_applied_exact_results() {
    let mut s = state(3);
    s["tasks"]["lane"]["operations"] = json!({"op":{"phase":"accepted",
        "assignment":"assignment","intent":{"wait":"result"}}});
    refusal(&mut s, &spec());
    let event = json!({"applied":true,"outcome":"complete","wait":"result",
        "binding":{"task":"lane","epoch":1,"assignment":"assignment"}});
    for (pointer, value) in [
        ("/applied", json!(false)),
        ("/outcome", json!("failed")),
        ("/outcome", json!("decision")),
        ("/outcome", json!(null)),
        ("/wait", json!("other")),
        ("/binding/task", json!("other")),
        ("/binding/epoch", json!(2)),
        ("/binding/assignment", json!("other")),
    ] {
        let mut bad = event.clone();
        *bad.pointer_mut(pointer).unwrap() = value;
        s["events"]["result"] = bad;
        refusal(&mut s, &spec());
    }
    s["events"]["result"] = event;
    authorize::append(&mut s, &spec(), 100.0).unwrap();
}

#[test]
fn capacity_conflict_and_overflow_are_atomic() {
    let mut s = state(3);
    for i in 0..63 {
        s["tasks"]["lane"]["authorization"][format!("id-{i}")] =
            json!({"action":"publication","source":"user"});
    }
    refusal(&mut s, &spec());
    let mut retry = spec();
    retry["id"] = json!("id-0");
    retry["source"] = json!("user");
    let before = s.clone();
    authorize::append(&mut s, &retry, 100.0).unwrap();
    assert_eq!(s, before);
    s["tasks"]["lane"]["authorization"]["extra"] = json!({});
    refusal(&mut s, &retry);
    s = state(3);
    s["tasks"]["lane"]["state_version"] = json!(i64::MAX);
    retry = spec();
    retry["state_version"] = json!(i64::MAX);
    refusal(&mut s, &retry);
}

#[test]
fn restored_authorization_does_not_bypass_publication_no_replay() {
    let mut s = state(3);
    let mut begin = json!({"task":"lane","state_version":12,"authorization":"publish"});
    let error = pr::publication_begin(&mut s, &begin, 100.0).unwrap_err();
    assert!(error
        .to_string()
        .contains("explicit publication authorization"));
    authorize::append(&mut s, &spec(), 100.0).unwrap();
    begin["state_version"] = json!(13);
    s["tasks"]["lane"]["pr"]["publication"]["phase"] = json!("sending");
    let before = s.clone();
    let error = pr::publication_begin(&mut s, &begin, 101.0).unwrap_err();
    assert!(error.to_string().contains("never replay"));
    assert_eq!(s, before);
}

#[test]
fn cli_uses_coordinator_identity_lock_and_persistent_state() {
    let temp = common::tempdir().unwrap();
    let guard = temp.path().join("guard");
    common::create_dir(&guard).unwrap();
    let checkout = temp.path().join("checkout");
    common::create_dir(&checkout).unwrap();
    util::atomic_write_json(
        &guard.join("state.json"),
        &json!({"version":2,"phase":"monitoring",
        "session":SESSION,"task":"project","role":"coordinator","cwd":checkout,"pane":"w1:p1"}),
    )
    .unwrap();
    let root = temp.path().join("queue");
    queue::init(&root, Some(&guard), 30).unwrap();
    let mut q = queue::load_queue(&root).unwrap();
    q["tasks"] = state(3)["tasks"].clone();
    queue::save(&root, &mut q).unwrap();
    let path = temp.path().join("spec.json");
    util::atomic_write_json(&path, &spec()).unwrap();
    let run = |session: Option<&str>| {
        let mut c = std::process::Command::new(env!("CARGO_BIN_EXE_herdr-flow"));
        c.args(["queue", "task-authorize", "--state-dir"])
            .arg(&root)
            .arg("--spec")
            .arg(&path)
            .env_remove("CODEX_THREAD_ID");
        if let Some(session) = session {
            c.env("CODEX_THREAD_ID", session);
        }
        c.output().unwrap()
    };
    let before = std::fs::read(root.join("queue.json")).unwrap();
    for session in [None, Some("22222222-2222-2222-2222-222222222222")] {
        assert!(!run(session).status.success());
        assert_eq!(std::fs::read(root.join("queue.json")).unwrap(), before);
    }
    let lock = queue::locked(&root, false).unwrap();
    assert!(!run(Some(SESSION)).status.success());
    drop(lock);
    assert_eq!(std::fs::read(root.join("queue.json")).unwrap(), before);
    let output = run(Some(SESSION));
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        queue::load_queue(&root).unwrap()["tasks"]["lane"]["state_version"],
        13
    );
    let committed = std::fs::read(root.join("queue.json")).unwrap();
    assert!(!run(Some(SESSION)).status.success());
    assert_eq!(std::fs::read(root.join("queue.json")).unwrap(), committed);
}
