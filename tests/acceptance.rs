//! Independent verdicts and atomic continuation; no native-agent simulation claims.
pub mod common;
use herdr_flow::{
    model::J,
    progress::{self, acceptance, lineage},
    util,
};
use serde_json::json;
const REVIEW: &str = "review-session";
fn mutate(s: &mut J, mut value: J) -> herdr_flow::Result<J> {
    value["task"] = json!("parent");
    value["state_version"] = s["tasks"]["parent"]["state_version"].clone();
    lineage::action(s, &value, 2.0)
}
fn request(name: &str, role: &str, input: &J) -> J {
    json!({"activity":name,"role":role,"input":input,"session_epoch":1,"result_wait":{
        "id":name,"epoch":1,"assignment":name,"owner":role,"kind":"signal","timeout_seconds":30,
        "reason":"result","condition":"execution finished","producer":{"adapter":"opencode","session":if role=="acceptance" {REVIEW} else {"developer-session"}}}})
}
fn fixture() -> (tempfile::TempDir, J, J) {
    let dir = common::tempdir().unwrap();
    let path = dir.path().join("artifact");
    std::fs::write(&path, "A").unwrap();
    let artifact =
        json!({"adapter":"file-sha256","id":util::pyjson::sha256_hex(b"A"),"location":path});
    let mut s = json!({"id":"queue","stopped":false,"tasks":{},"waits":{},"events":{}});
    lineage::action(&mut s,&json!({"kind":"plan","task":"parent","goal":"deliver requested A",
        "basis":{"original":["request"],"changes":[],"specifications":["spec"],"endpoint":"customer","confirmation":"user"},
        "authorization":[{"id":"develop","action":"development","source":"user"}]}),1.0).unwrap();
    mutate(&mut s, json!({"kind":"artifact","artifact":artifact})).unwrap();
    mutate(
        &mut s,
        json!({"kind":"candidate","candidate":artifact["id"],"reason":"review"}),
    )
    .unwrap();
    let mut r = request("review", "acceptance", &artifact["id"]);
    r["kind"] = json!("activity");
    mutate(&mut s, r).unwrap();
    (dir, s, artifact)
}
fn report(conclusion: &str) -> J {
    let mut r = json!({"conclusion":conclusion,"scope":"requested document", "checks":[{"id":"content","criterion":"goal","source":"request","status":"passed","evidence":"checked artifact"}],
        "gaps":[],"reuses":[],"resolutions":[]});
    match conclusion {
        "failed" => {
            r["checks"][0]["status"] = json!("failed");
            r["gaps"] = json!([{"id":"content","criterion":"goal","source":"request","evidence":"wrong content","release_condition":"correct content"}]);
        }
        "unable" => {
            r["checks"][0]["status"] = json!("blocked");
            r["blocker"] = json!("fixture unavailable");
            r["restore_condition"] = json!("fixture accessible");
        }
        "ambiguous" => {
            r["checks"][0]["status"] = json!("blocked");
            r["question"] = json!("which endpoint?");
            r["unresolved_after_review"] = json!("request and spec conflict");
            r["agreements_checked"] = json!(["request", "spec", "user"]);
        }
        _ => {}
    };
    r
}
fn submit(s: &mut J, name: &str, r: &J) -> herdr_flow::Result<J> {
    acceptance::submit(s, name, &json!({"adapter":"opencode","session":REVIEW}), r)
}
fn event(s: &mut J, name: &str, outcome: &str) -> String {
    let id = format!("event-{name}");
    let w = s["waits"][name].clone();
    let mut e = json!({"id":id,"task":name,"wait":name,"acknowledged":true,"outcome":outcome});
    progress::attach(s, &mut e, &w, 3.0).unwrap();
    s["events"][&id] = e;
    id
}
fn application(s: &J, name: &str, event: &str, next: &str) -> J {
    json!({"task":name,"state_version":s["tasks"][name]["state_version"],"event":event,"verdict":s["tasks"][name]["verdict"]["id"],
        "next_action":{"kind":next,"owner":"coordinator","reason":"follow independent report","condition":"explicit resolution"}})
}
#[test]
fn four_conclusions_apply_atomically_and_cannot_be_rewritten() {
    for (verdict, next) in [
        ("passed", "delivery"),
        ("failed", "repair"),
        ("unable", "restore"),
        ("ambiguous", "clarify"),
    ] {
        let (_d, mut s, _) = fixture();
        let r = report(verdict);
        let sealed = submit(&mut s, "review", &r).unwrap();
        assert_eq!(submit(&mut s, "review", &r).unwrap(), sealed);
        let mut changed = r.clone();
        changed["scope"] = json!("changed");
        assert!(submit(&mut s, "review", &changed).is_err());
        let e = event(&mut s, "review", "complete");
        let spec = application(&s, "review", &e, next);
        let before = s.clone();
        let mut bad = spec.clone();
        bad["acceptance"] = json!({"goal":"override"});
        assert!(progress::apply(&mut s, &bad, 4.0).is_err());
        assert_eq!(s, before);
        bad = spec.clone();
        bad["next_action"]["kind"] = json!("complete");
        assert!(progress::apply(&mut s, &bad, 4.0).is_err());
        progress::apply(&mut s, &spec, 4.0).unwrap();
        assert_eq!(s["tasks"]["parent"]["continuation"]["action"]["kind"], next);
        assert_eq!(
            !s["tasks"]["parent"]["approved"].is_null(),
            verdict == "passed"
        );
        assert_eq!(
            s["tasks"]["parent"]["requirements"]["1"]["authorization"]["develop"]["source"],
            "user"
        );
        assert!(progress::apply(&mut s, &spec, 5.0).is_err());
    }
}

#[test]
fn execution_end_ack_and_runtime_failure_cannot_grant_acceptance() {
    for outcome in ["complete", "failed", "decision"] {
        let (_d, mut s, _) = fixture();
        let e = event(&mut s, "review", outcome);
        let mut spec = application(&s, "review", &e, "delivery");
        assert!(progress::apply(&mut s, &spec, 4.0).is_err());
        spec["execution_problem"] = json!("execution failed or report absent");
        spec["next_action"]["kind"] = json!("restore");
        progress::apply(&mut s, &spec, 4.0).unwrap();
        assert!(s["tasks"]["parent"]["approved"].is_null());
        assert!(s["tasks"]["parent"]["verdicts"]["review"].is_null());
        assert_eq!(
            s["tasks"]["parent"]["continuation"]["action"]["kind"],
            "restore"
        );
    }
}
#[test]
fn basis_scope_identity_and_prior_agreements_are_checked() {
    let (_d, mut s, _) = fixture();
    let original = s.clone();
    assert!(acceptance::submit(
        &mut s,
        "review",
        &json!({"adapter":"codex","session":REVIEW}),
        &report("passed")
    )
    .is_err());
    for field in ["source", "criterion", "status"] {
        let mut r = report("passed");
        r["checks"][0][field] = json!("invented");
        assert!(submit(&mut s, "review", &r).is_err());
    }
    let mut r = report("passed");
    r["checks"] = json!([]);
    assert!(submit(&mut s, "review", &r).is_err());
    r = report("ambiguous");
    r["agreements_checked"] = json!(["request"]);
    assert!(submit(&mut s, "review", &r).is_err());
    r = report("failed");
    r["gaps"][0]["release_condition"] = json!("");
    assert!(submit(&mut s, "review", &r).is_err());
    assert_eq!(s, original);
}
#[test]
fn restart_registers_repair_once_and_reverification_must_close_gaps() {
    let (dir, mut s, a) = fixture();
    submit(&mut s, "review", &report("failed")).unwrap();
    let e = event(&mut s, "review", "complete");
    let spec = application(&s, "review", &e, "repair");
    progress::apply(&mut s, &spec, 4.0).unwrap();
    s = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
    let resume = json!({"kind":"continue","task":"parent","state_version":s["tasks"]["parent"]["state_version"],"intent":"review","authorization":"develop","action":"development","resolution":"repair existing gap", "activity":request("repair","development",&a["id"])});
    let child = lineage::action(&mut s, &resume, 5.0).unwrap();
    let snapshot = s.clone();
    assert_eq!(lineage::action(&mut s, &resume, 6.0).unwrap(), child);
    assert_eq!(snapshot, s);
    let mut bad = resume.clone();
    bad["activity"]["activity"] = json!("duplicate");
    assert!(lineage::action(&mut s, &bad, 6.0).is_err());
    let path = dir.path().join("b");
    std::fs::write(&path, "B").unwrap();
    let b = json!({"adapter":"file-sha256","id":util::pyjson::sha256_hex(b"B"),"location":path,"input":a["id"]});
    let e = event(&mut s, "repair", "complete");
    let spec = json!({"task":"repair","state_version":s["tasks"]["repair"]["state_version"],"event":e,"output":b,"acceptance":{"goal":"repair verified"},"next_action":{"kind":"complete"}});
    progress::apply(&mut s, &spec, 7.0).unwrap();
    mutate(
        &mut s,
        json!({"kind":"candidate","candidate":b["id"],"reason":"retest repaired artifact"}),
    )
    .unwrap();
    let mut r = request("retest", "acceptance", &b["id"]);
    r["kind"] = json!("activity");
    mutate(&mut s, r).unwrap();
    let mut pass = report("passed");
    assert!(submit(&mut s, "retest", &pass).is_err());
    pass["resolutions"] = json!([{"activity":"review","gap":"content","evidence":"new artifact check","explanation":"correct content now meets request"}]);
    pass["reuses"] = json!([{"activity":"review","check":"content","evidence":"checked artifact","applicability":"same requirement"}]);
    assert!(submit(&mut s, "retest", &pass).is_err());
    pass["reuses"][0]["equivalence_evidence"] = json!("unchanged relevant source verified");
    submit(&mut s, "retest", &pass).unwrap();
    let e = event(&mut s, "retest", "complete");
    let spec = application(&s, "retest", &e, "delivery");
    progress::apply(&mut s, &spec, 8.0).unwrap();
    assert_eq!(s["tasks"]["parent"]["approved"]["candidate"], b["id"]);
    assert_eq!(s["tasks"]["parent"]["open_gaps"], json!({}));
    mutate(
        &mut s,
        json!({"kind":"candidate","candidate":a["id"],"reason":"replace again"}),
    )
    .unwrap();
    assert!(s["tasks"]["parent"]["approved"].is_null());
}
#[test]
fn ambiguity_requires_confirmed_revision_before_resume() {
    let (_d, mut s, a) = fixture();
    submit(&mut s, "review", &report("ambiguous")).unwrap();
    let e = event(&mut s, "review", "complete");
    let spec = application(&s, "review", &e, "clarify");
    progress::apply(&mut s, &spec, 4.0).unwrap();
    let mut resume = json!({"kind":"continue","task":"parent","state_version":s["tasks"]["parent"]["state_version"],"intent":"review","authorization":"develop","action":"development","resolution":"user selected endpoint","activity":request("clarified","acceptance",&a["id"])});
    assert!(lineage::action(&mut s, &resume, 5.0).is_err());
    let previous = s["tasks"]["parent"]["requirements"]["1"].clone();
    mutate(&mut s,json!({"kind":"revise","goal":"deliver requested A", "basis":{"original":["request"],"changes":["confirmed endpoint"],"specifications":["spec"],"endpoint":"customer","confirmation":"user confirmation"}})).unwrap();
    mutate(&mut s,json!({"kind":"candidate","candidate":a["id"],"reason":"review under clarified requirements"})).unwrap();
    resume["state_version"] = s["tasks"]["parent"]["state_version"].clone();
    lineage::action(&mut s, &resume, 6.0).unwrap();
    assert_eq!(s["tasks"]["clarified"]["lineage"]["revision"], 2);
    assert_eq!(
        previous["authorization"],
        s["tasks"]["parent"]["requirements"]["2"]["authorization"]
    );
}

#[test]
fn unsuccessful_execution_cannot_promote_a_previously_written_report() {
    let (_d, mut s, _) = fixture();
    submit(&mut s, "review", &report("passed")).unwrap();
    let e = event(&mut s, "review", "failed");
    let mut spec = application(&s, "review", &e, "delivery");
    assert!(progress::apply(&mut s, &spec, 4.0).is_err());
    spec["verdict"] = json!(null);
    spec["execution_problem"] = json!("native execution failed");
    spec["next_action"]["kind"] = json!("restore");
    progress::apply(&mut s, &spec, 4.0).unwrap();
    assert!(s["tasks"]["parent"]["verdicts"]["review"].is_null());
    assert!(s["tasks"]["parent"]["approved"].is_null());
}
#[test]
fn restoration_requires_existing_authorization_and_retains_current_binding() {
    let (_d, mut s, a) = fixture();
    submit(&mut s, "review", &report("unable")).unwrap();
    let e = event(&mut s, "review", "complete");
    let spec = application(&s, "review", &e, "restore");
    progress::apply(&mut s, &spec, 4.0).unwrap();
    let mut resume = json!({"kind":"continue","task":"parent","state_version":s["tasks"]["parent"]["state_version"],"intent":"review","resolution":"fixture restored", "activity":request("restored","acceptance",&a["id"])});
    let before = s.clone();
    assert!(lineage::action(&mut s, &resume, 5.0).is_err());
    assert_eq!(s, before);
    resume["authorization"] = json!("develop");
    resume["action"] = json!("development");
    let child = lineage::action(&mut s, &resume, 5.0).unwrap();
    assert_eq!(
        child["lineage"]["round"],
        s["tasks"]["review"]["lineage"]["round"]
    );
    assert_eq!(lineage::action(&mut s, &resume, 6.0).unwrap(), child);
}
#[test]
fn changed_reviewer_and_changed_artifact_reject_application_without_consuming_result() {
    let (_d, mut s, a) = fixture();
    submit(&mut s, "review", &report("passed")).unwrap();
    let e = event(&mut s, "review", "complete");
    let spec = application(&s, "review", &e, "delivery");
    s["waits"]["review"]["producer"]["session"] = json!("other-reviewer");
    let before = s.clone();
    assert!(progress::apply(&mut s, &spec, 4.0).is_err());
    assert_eq!(s, before);
    s["waits"]["review"]["producer"]["session"] = json!(REVIEW);
    std::fs::write(a["location"].as_str().unwrap(), "changed").unwrap();
    let before = s.clone();
    assert!(progress::apply(&mut s, &spec, 4.0).is_err());
    assert_eq!(s, before);
}

#[test]
fn independent_reviewer_cannot_be_the_artifact_producer() {
    let (_d, mut s, a) = fixture();
    let mut dev = request("developer", "development", &a["id"]);
    dev["kind"] = json!("activity");
    dev["result_wait"]["producer"]["session"] = json!(REVIEW);
    mutate(&mut s, dev).unwrap();
    s["tasks"]["parent"]["artifacts"][a["id"].as_str().unwrap()]["activity"] = json!("developer");
    assert!(submit(&mut s, "review", &report("passed"))
        .unwrap_err()
        .message
        .contains("independent reviewer"));
}
#[test]
fn existing_authorization_must_be_reviewed_before_requesting_clarification() {
    let (_d, mut s, _) = fixture();
    let mut r = report("ambiguous");
    r["agreements_checked"] = json!(["request", "spec"]);
    assert!(submit(&mut s, "review", &r).is_err());
}
