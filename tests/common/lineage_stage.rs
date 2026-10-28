use super::*;

fn staged_spec() -> J {
    json!({"kind":"stage","id":"draft","scope":"deliver the reviewed draft","criteria":["draft"]})
}

fn planned() -> J {
    let mut s = json!({"id":"queue","stopped":false,"tasks":{},"events":{},"waits":{}});
    let mut p = plan();
    p["acceptance"] = json!([
        {"id":"draft","description":"draft is correct","required":true},
        {"id":"final","description":"final report includes supporting evidence","required":true}
    ]);
    lineage::action(&mut s, &p, 1.0).unwrap();
    s
}

#[test]
fn stage_inherits_selected_criteria_and_keeps_the_overall_agreement_in_native_input() {
    let mut s = planned();
    let original = s["tasks"]["project"]["requirements"].clone();
    mutate(&mut s, staged_spec()).unwrap();
    let dir = common::tempdir().unwrap();
    let a = file(dir.path(), "draft", "draft", None);
    add(&mut s, &a);
    select(&mut s, &a);
    for role in ["development", "acceptance"] {
        activity(&mut s, role, role, &a);
        assert_eq!(s["tasks"][role]["lineage"]["scope_epoch"], 1);
        assert_eq!(s["tasks"][role]["acceptance"].as_object().unwrap().len(), 1);
        assert!(s["tasks"][role]["acceptance"]["final"].is_null());
        let packet =
            herdr_flow::task_runtime::native::packet(&s, &s["waits"][role], "review draft", &[])
                .unwrap();
        assert_eq!(
            packet["requirement"]["criteria"],
            json!([original["1"]["criteria"][0]])
        );
        assert_eq!(packet["requirement"]["goal"], "deliver the reviewed draft");
        assert_eq!(packet["requirement"]["basis"], original["1"]["basis"]);
        assert_eq!(
            packet["requirement"]["authorization"],
            original["1"]["authorization"]
        );
        assert_eq!(packet["overall_requirement"], original["1"]);
    }
    assert_eq!(s["tasks"]["project"]["requirements"], original);
}

#[test]
fn stage_rejects_new_criteria_authority_duplicate_scope_and_malformed_epoch_atomically() {
    let s = planned();
    for change in [
        json!({"criteria":["invented"]}),
        json!({"criteria":["draft","draft"]}),
        json!({"criteria":[]}),
        json!({"criteria":[null]}),
        json!({"criteria":"draft"}),
        json!({"authorization":[]}),
        json!({"goal":"weaker agreement"}),
        json!({"scope":""}),
    ] {
        let mut spec = staged_spec();
        spec.as_object_mut()
            .unwrap()
            .extend(change.as_object().unwrap().clone());
        let mut q = s.clone();
        assert!(mutate(&mut q, spec).is_err());
        assert_eq!(q, s);
    }
    for epoch in [json!(null), json!(u64::MAX)] {
        let mut q = s.clone();
        q["tasks"]["project"]["scope_epoch"] = epoch;
        let before = q.clone();
        assert!(mutate(&mut q, staged_spec()).is_err());
        assert_eq!(q, before);
    }
    let mut q = s;
    mutate(&mut q, staged_spec()).unwrap();
    let before = q.clone();
    assert!(mutate(&mut q, staged_spec()).is_err());
    assert_eq!(q, before);
}

#[test]
fn stage_waits_for_original_execution_and_refuses_pending_writers_or_unknown_operations() {
    let dir = common::tempdir().unwrap();
    let a = file(dir.path(), "a", "A", None);
    let mut s = state();
    add(&mut s, &a);
    activity(&mut s, "dev", "development", &a);
    let spec = json!({"kind":"stage","id":"one","scope":"first delivery","criteria":["goal"]});
    let before = s.clone();
    assert!(mutate(&mut s, spec.clone()).is_err());
    assert_eq!(s, before);
    let e = event(&mut s, "dev");
    assert!(mutate(&mut s, spec.clone()).is_err());
    let apply = apply_spec(&s, "dev", &e);
    progress::apply(&mut s, &apply, 4.0).unwrap();
    for change in [
        json!({"writer":{"active":true}}),
        json!({"operations":{"send":{"phase":"unknown"}}}),
        json!({"pending":["unapplied"]}),
        json!({"phase":"running"}),
    ] {
        let mut q = s.clone();
        q["tasks"]["dev"]
            .as_object_mut()
            .unwrap()
            .extend(change.as_object().unwrap().clone());
        let before = q.clone();
        assert!(mutate(&mut q, spec.clone()).is_err());
        assert_eq!(q, before);
    }
    let mut q = s.clone();
    q["events"][&e]["applied"] = json!(false);
    assert!(mutate(&mut q, spec.clone()).is_err());
    mutate(&mut s, spec).unwrap();
    assert!(!lineage::current(&s, &s["tasks"]["dev"]));
    assert!(!progress::current_event(&s, &s["events"][&e]));
    assert!(
        herdr_flow::task_runtime::native::packet(&s, &s["waits"]["dev"], "stale", &[]).is_err()
    );
}

#[test]
fn explicit_requirement_revision_supersedes_stage_and_late_result_is_withdrawn() {
    let mut s = planned();
    mutate(&mut s, staged_spec()).unwrap();
    let dir = common::tempdir().unwrap();
    let a = file(dir.path(), "a", "draft", None);
    add(&mut s, &a);
    activity(&mut s, "dev", "development", &a);
    let mut revision = plan();
    revision["kind"] = json!("revise");
    revision["basis"]["changes"] = json!(["explicitly confirmed overall scope"]);
    mutate(&mut s, revision).unwrap();
    assert!(s["tasks"]["project"]["stage"].is_null());
    assert_eq!(s["tasks"]["project"]["scope_epoch"], 2);
    assert!(!lineage::current(&s, &s["tasks"]["dev"]));
    let e = event(&mut s, "dev");
    assert_eq!(s["events"][e]["withdrawn"], true);
}

#[test]
fn settled_execution_failure_does_not_pin_future_stages_but_unknown_decisions_do() {
    let dir = common::tempdir().unwrap();
    let a = file(dir.path(), "a", "A", None);
    for outcome in ["failed", "decision"] {
        let mut s = state();
        add(&mut s, &a);
        activity(&mut s, "dev", "development", &a);
        let e = event(&mut s, "dev");
        s["events"][&e]["outcome"] = json!(outcome);
        let spec = json!({"task":"dev","state_version":s["tasks"]["dev"]["state_version"],"event":e,
            "next_action":{"kind":"blocked","owner":"coordinator","reason":"inspect original execution","condition":"settled outcome"}});
        progress::apply(&mut s, &spec, 4.0).unwrap();
        let before = s.clone();
        let next = mutate(
            &mut s,
            json!({"kind":"stage","id":"next","scope":"next delivery","criteria":["goal"]}),
        );
        if outcome == "failed" {
            next.unwrap();
            assert!(!lineage::current(&s, &s["tasks"]["dev"]));
            assert!(s["tasks"]["project"]["approved"].is_null());
        } else {
            assert!(next.is_err());
            assert_eq!(s, before);
        }
    }
}
