//! Deterministic contract fixtures, not live lane or Gate acceptance.
pub mod common;
use herdr_flow::{model::J, pr, progress, queue};
use serde_json::{json, Value};

const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const SESSION: &str = "11111111-1111-1111-1111-111111111111";

fn independent() -> J {
    json!({"version":1,"kind":"independent","admission_evidence":"/evidence/decision"})
}
fn root(previous: J) -> J {
    json!({"version":1,"kind":"root","lane":"lane","previous":previous,"admission_evidence":"/evidence/decision"})
}
fn member() -> J {
    json!({"version":1,"kind":"member","root":"root","root_epoch":1,"admission_evidence":"/evidence/decision"})
}
fn prior() -> J {
    json!({"task":"source","epoch":1,"candidate":B,"receipt_id":"receipt-one"})
}
fn state() -> J {
    json!({"version":1,"id":"q","stopped":false,"waits":{},"events":{},"tasks":{
        "lane":{"task":"lane","epoch":1,"state_version":1,"control":"running",
          "lane":{"repo":"o/r","host":"github.com","base_ref":"main","pending":[],"active":null,"capacity":3}}
    }})
}
fn spec(s: &mut J, name: &str, claim: J, candidate: &str) -> J {
    s["waits"][name] = json!({"id":name,"task":name,"epoch":1,"assignment":format!("{name}-a1"),
        "candidate":candidate,"owner":"developer","kind":"signal","condition":"done","closed":false,
        "producer_session":SESSION});
    json!({"task":name,"wait":name,"goal":"bounded task","claim":claim,
        "acceptance":[{"id":"done","description":"verified"}],
        "authorization":[{"id":"develop","action":"development","source":"user"}]})
}
fn add(s: &mut J, name: &str, claim: J, candidate: &str) -> J {
    let input = spec(s, name, claim, candidate);
    progress::register(s, &input, 1.0).unwrap()
}
fn accepted() -> J {
    let mut s = state();
    add(&mut s, "root", root(Value::Null), A);
    add(&mut s, "source", member(), B);
    s["tasks"]["root"]["phase"] = json!("blocked");
    s["tasks"]["source"]["phase"] = json!("blocked");
    s["tasks"]["source"]["batch_delivery"] = json!({"owner":"lane","phase":"accepted","receipt_id":"receipt-one","head":B,"own_commits":[B]});
    s["tasks"]["lane"]["lane"]["pending"] = json!([{"task":"source","epoch":1,"head":B,"receipt_id":"receipt-one","own_commits":[B],
        "original_base":A,"checkout":"/fixture/source","host":"github.com","repo":"o/r"}]);
    s
}

#[test]
fn explicit_first_independent_and_member_preserve_parallelism() {
    let mut s = state();
    s["tasks"]["lane"]["lane"]["active"] = json!({"id":"ci-running"});
    s["tasks"]["lane"]["lane"]["pending"] = json!([{}, {}, {}]);
    let r = add(&mut s, "root", root(Value::Null), A);
    let m = add(&mut s, "source", member(), B);
    add(&mut s, "unrelated", independent(), A);
    assert_eq!(r["schema_version"], 3);
    assert_eq!(m["claim"], member());
    assert_ne!(r["candidate"], m["candidate"]);
    assert_eq!(
        s["tasks"]["lane"]["lane"]["pending"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
}

#[test]
fn missing_malformed_unknown_and_indirect_admissions_are_atomic() {
    let mut s = state();
    add(&mut s, "root", root(Value::Null), A);
    add(&mut s, "source", member(), B);
    let bad = [
        Value::Null,
        json!({}),
        json!({"version":2,"kind":"independent","admission_evidence":"/e"}),
        json!({"version":1,"kind":"independent","admission_evidence":"relative"}),
        json!({"version":1,"kind":"independent","admission_evidence":"/e","session":SESSION}),
        json!({"version":1,"kind":"root","lane":"lane","admission_evidence":"/e"}),
        json!({"version":1,"kind":"member","root":"source","root_epoch":1,"admission_evidence":"/e"}),
        json!({"version":1,"kind":"member","root":"root","root_epoch":2,"admission_evidence":"/e"}),
    ];
    for c in bad {
        let input = spec(&mut s, "next", c, A);
        let before = s.clone();
        assert!(progress::register(&mut s, &input, 2.0).is_err());
        assert_eq!(s, before);
    }
    let mut input = spec(&mut s, "next", independent(), A);
    input.as_object_mut().unwrap().remove("claim");
    let before = s.clone();
    assert!(progress::register(&mut s, &input, 2.0).is_err());
    assert_eq!(s, before);
}

#[test]
fn exact_receipt_consumes_once_not_per_session_or_phase() {
    let mut s = accepted();
    s["tasks"]["lane"]["lane"]["active"] = json!({"id":"other-ci"});
    let next = add(&mut s, "next", root(prior()), A);
    assert_eq!(
        s["tasks"]["source"]["batch_delivery"]["claimed_by"],
        json!({"task":"next","epoch":1})
    );
    assert_eq!(s["tasks"]["source"]["state_version"], 2);
    let input = spec(&mut s, "another", root(prior()), B);
    let before = s.clone();
    assert!(progress::register(&mut s, &input, 2.0).is_err());
    assert_eq!(s, before);
    // Pending can fill after admission; already admitted execution remains valid.
    s["tasks"]["lane"]["lane"]["pending"]
        .as_array_mut()
        .unwrap()
        .extend([json!({"task":"other"}), json!({"task":"third"})]);
    progress::claim::execution(&s, &next).unwrap();
    let saved: J = serde_json::from_slice(&serde_json::to_vec(&s).unwrap()).unwrap();
    progress::claim::validate(&saved, &saved["tasks"]["next"]).unwrap();
}

#[test]
fn full_pending_and_late_writer_failure_do_not_consume() {
    let mut s = accepted();
    let input = spec(&mut s, "next", root(prior()), A);
    s["tasks"]["lane"]["lane"]["capacity"] = json!(1);
    let before = s.clone();
    assert!(progress::register(&mut s, &input, 2.0).is_err());
    assert_eq!(s, before);
    s["tasks"]["lane"]["lane"]["capacity"] = json!(3);
    let mut input = input;
    input["workspace"] = json!("relative/invalid");
    input["execution_evidence"] = json!("inspected");
    let before = s.clone();
    assert!(progress::register(&mut s, &input, 2.0).is_err());
    assert_eq!(
        s, before,
        "late writer failure must roll back receipt consumption"
    );
}

#[test]
fn consumption_rechecks_late_work_on_every_source_root_task_atomically() {
    for task in ["root", "source"] {
        for (field, value) in [
            ("pending", json!(["late-result"])),
            ("operations", json!({"late":{"phase":"prepared"}})),
            ("operations", json!({"late":{"phase":"sending"}})),
            ("writer", json!({"workspace":"/fixture/late"})),
            ("phase", json!("waiting")),
        ] {
            let mut s = accepted();
            // Intake has already accepted the source; work arrives before consumption.
            s["tasks"][task][field] = value;
            let input = spec(&mut s, "next", root(prior()), A);
            let before = s.clone();
            assert!(
                progress::register(&mut s, &input, 2.0).is_err(),
                "{task}/{field}"
            );
            assert_eq!(
                s, before,
                "rejection must preserve every version and marker"
            );
            assert!(s["tasks"].get("next").is_none());
            assert!(s["tasks"]["source"]["batch_delivery"]
                .get("claimed_by")
                .is_none());
        }
    }
}

#[test]
fn held_rechecks_marker_target_lane_and_ancestry() {
    let mut s = accepted();
    add(&mut s, "next", root(prior()), A);
    progress::claim::held(&s, &s["tasks"]["source"]).unwrap();
    let mut other = s["tasks"]["lane"].clone();
    other["task"] = json!("other-lane");
    s["tasks"]["other-lane"] = other;
    s["tasks"]["next"]["claim"]["lane"] = json!("other-lane");
    assert!(progress::claim::held(&s, &s["tasks"]["source"]).is_err());
}

#[test]
fn wrong_receipt_tuple_and_member_provenance_are_rejected() {
    let mutations = [
        ("/tasks/source/batch_delivery/owner", json!("other")),
        ("/tasks/source/candidate", json!(A)),
        ("/tasks/source/epoch", json!(2)),
        ("/tasks/source/batch_delivery/receipt_id", json!("foreign")),
        ("/tasks/lane/lane/pending/0/epoch", json!(2)),
        ("/tasks/lane/lane/pending/0/head", json!(A)),
        ("/tasks/lane/lane/pending/0/own_commits", json!([A])),
        ("/tasks/lane/lane/pending/0/host", json!("foreign")),
        ("/tasks/lane/lane/pending/0/original_base", json!("short")),
        ("/tasks/source/control", json!("cancelled")),
        ("/tasks/root/control", json!("paused")),
        ("/tasks/root/claim/lane", json!("missing")),
    ];
    for (pointer, value) in mutations {
        let mut s = accepted();
        *s.pointer_mut(pointer).unwrap() = value;
        let input = spec(&mut s, "next", root(prior()), A);
        let before = s.clone();
        assert!(
            progress::register(&mut s, &input, 2.0).is_err(),
            "{pointer}"
        );
        assert_eq!(s, before);
    }
}

#[test]
fn legacy_receipts_cannot_authorize_and_cancellation_never_refunds() {
    let mut s = accepted();
    s["tasks"]["source"]
        .as_object_mut()
        .unwrap()
        .remove("claim");
    s["tasks"]["source"]["schema_version"] = json!(1);
    let input = spec(&mut s, "next", root(prior()), A);
    assert!(progress::register(&mut s, &input, 2.0).is_err());
    let mut s = accepted();
    add(&mut s, "next", root(prior()), A);
    progress::control(
        &mut s,
        Some(&json!("next")),
        Some(&json!(1)),
        Some(&json!("cancelled")),
        Some(&json!("operator")),
        3.0,
    )
    .unwrap();
    progress::release(
        &mut s,
        &json!({"task":"next","state_version":2,"evidence":"stopped"}),
        4.0,
    )
    .unwrap();
    assert_eq!(
        s["tasks"]["source"]["batch_delivery"]["claimed_by"]["task"],
        "next"
    );
    let input = spec(&mut s, "replacement", root(prior()), A);
    assert!(progress::register(&mut s, &input, 5.0).is_err());
}

#[test]
fn landing_does_not_need_history_or_mint_a_new_receipt() {
    let mut s = accepted();
    s["tasks"]["lane"]["lane"]["pending"] = json!([]);
    s["tasks"]["source"]["batch_delivery"]["phase"] = json!("landed");
    s["tasks"]["source"]["batch_delivery"]["batch"] = json!("batch");
    s["tasks"]["source"]["batch_delivery"]["landing"] = json!({"head":B,"batch":"batch","batch_head":B,"tree":A,"commit":B,"repo":"o/r","base_ref":"main"});
    add(&mut s, "next", root(prior()), A);
    assert_eq!(
        s["tasks"]["source"]["batch_delivery"]["receipt_id"],
        "receipt-one"
    );
}

#[test]
fn multi_source_and_unfinished_root_are_blocked_at_offer() {
    let mut s = state();
    add(&mut s, "root", root(Value::Null), A);
    add(&mut s, "source", member(), B);
    s["tasks"]["root"]["phase"] = json!("blocked");
    s["tasks"]["source"]["phase"] = json!("blocked");
    let source = s["tasks"]["source"].clone();
    let owner = s["tasks"]["lane"].clone();
    progress::claim::offer(&s, &owner, &source).unwrap();
    s["tasks"]["root"]["writer"] = json!({"workspace":"/fixture/root"});
    assert!(progress::claim::offer(&s, &owner, &source).is_err());
    s["tasks"]["root"]["writer"] = Value::Null;
    s["tasks"]["root"]["operations"]["op"] = json!({"phase":"sending"});
    assert!(progress::claim::offer(&s, &owner, &source).is_err());
    let mut s = accepted();
    let input = spec(&mut s, "second", member(), A);
    assert!(progress::register(&mut s, &input, 2.0).is_err());
    assert!(progress::claim::offer(&s, &s["tasks"]["lane"], &s["tasks"]["root"]).is_err());
}

#[test]
fn execution_entry_points_do_not_bypass_an_accepted_root() {
    let mut s = accepted();
    s["tasks"]["root"]["phase"] = json!("next_pending");
    s["tasks"]["root"]["operations"]["op"] = json!({"phase":"prepared"});
    assert!(progress::begin(
        &mut s,
        Some(&json!("root")),
        Some(&json!(1)),
        Some(&json!("op")),
        2.0
    )
    .is_err());
    assert!(progress::reserve_writer(&s, "/fixture/other", "a", SESSION, "root").is_err());
    assert!(pr::prepare(&mut s, &json!({"task":"root","state_version":1}), 2.0).is_err());
    assert!(pr::publication_begin(&mut s, &json!({"task":"root","state_version":1}), 2.0).is_err());
    assert!(progress::lineage::action(
        &mut s,
        &json!({"kind":"migrate","task":"root","state_version":1}),
        2.0
    )
    .is_err());
    s["tasks"]["root"]["phase"] = json!("blocked");
    assert!(progress::resume(&mut s, &json!({"task":"root","state_version":1}), 2.0).is_err());
    s["tasks"]["root"]["operations"]["op"] =
        json!({"phase":"sending","session":SESSION,"assignment":"root-a1"});
    s["tasks"]["root"]["phase"] = json!("next_pending");
    assert!(progress::accept(&mut s,&json!({"task":"root","state_version":1,"operation":"op","assignment":"root-a1","evidence":"received"}),SESSION,2.0).is_err());
}

#[test]
fn task_accept_special_queue_route_enforces_claim_before_save() {
    let tmp = common::tempdir().unwrap();
    let dir = tmp.path().join("queue");
    let mut s = accepted();
    s["tasks"]["root"]["phase"] = json!("next_pending");
    s["tasks"]["root"]["operations"]["op"] =
        json!({"phase":"sending","session":SESSION,"assignment":"root-a1"});
    queue::save(&dir, &mut s).unwrap();
    let before = std::fs::read(dir.join("queue.json")).unwrap();
    let _lock = queue::locked(&dir, false).unwrap();
    let spec = json!({"task":"root","state_version":1,"operation":"op","assignment":"root-a1","evidence":"received"});
    let error = queue::task_action(&dir, "task-accept", &spec, Some(SESSION), &|_| {
        panic!("receiver factory must not be called")
    })
    .unwrap_err();
    assert!(error.message.contains("accepted handoff"), "{error:?}");
    assert_eq!(std::fs::read(dir.join("queue.json")).unwrap(), before);
}

#[test]
fn apply_dispatch_and_wait_cannot_restart_accepted_root_but_block_can() {
    let mut s = accepted();
    s["tasks"]["root"]["phase"] = json!("result_pending");
    s["tasks"]["root"]["pending"] = json!(["e"]);
    s["events"]["e"] = json!({"id":"e","wait":"root","task":"root","epoch":1,"acknowledged":true,
        "binding":{"task":"root","epoch":1,"assignment":"root-a1","candidate":A}});
    for kind in ["dispatch", "wait"] {
        let before = s.clone();
        let input =
            json!({"task":"root","state_version":1,"event":"e","next_action":{"kind":kind}});
        let error = progress::apply(&mut s, &input, 2.0).unwrap_err();
        assert!(error.message.contains("accepted handoff"), "{error:?}");
        assert_eq!(s, before);
    }
    let input = json!({"task":"root","state_version":1,"event":"e","next_action":{"kind":"blocked","owner":"coordinator","reason":"await landing","condition":"landing verified"}});
    progress::apply(&mut s, &input, 2.0).unwrap();
    assert_eq!(s["tasks"]["root"]["phase"], "blocked");
}

#[test]
fn consumed_marker_schema_and_ancestor_changes_fail_closed() {
    let mut s = accepted();
    add(&mut s, "next", root(prior()), A);
    for (path, value) in [
        (
            "/tasks/source/batch_delivery/claimed_by/task",
            json!("missing"),
        ),
        ("/tasks/next/claim/version", json!(2)),
        ("/tasks/next/schema_version", json!(1)),
        ("/tasks/root/control", json!("cancelled")),
        ("/tasks/source/control", json!("paused")),
        ("/tasks/lane/control", json!("paused")),
    ] {
        let mut changed = s.clone();
        *changed.pointer_mut(path).unwrap() = value;
        assert!(
            progress::claim::execution(&changed, &changed["tasks"]["next"]).is_err(),
            "{path}"
        );
    }
    s["tasks"]["next"].as_object_mut().unwrap().remove("claim");
    assert!(progress::task(&s, Some(&json!("next")), Some(&json!(1))).is_err());
}

#[test]
fn same_root_repair_and_historical_applied_operation_do_not_consume() {
    let mut s = state();
    add(&mut s, "root", root(Value::Null), A);
    s["tasks"]["root"]["phase"] = json!("next_pending");
    s["tasks"]["root"]["operations"]["repair"] = json!({"phase":"prepared","session":SESSION,"assignment":"root-a1","intent":{"wait":"root"}});
    progress::begin(
        &mut s,
        Some(&json!("root")),
        Some(&json!(1)),
        Some(&json!("repair")),
        2.0,
    )
    .unwrap();
    progress::accept(&mut s,&json!({"task":"root","state_version":2,"operation":"repair","assignment":"root-a1","evidence":"received"}),SESSION,3.0).unwrap();
    s["tasks"]["root"]["phase"] = json!("blocked");
    s["events"]["repair-done"] = json!({"applied":true,"wait":"root","binding":{"task":"root","epoch":1,"assignment":"root-a1"}});
    progress::claim::offer(&s, &s["tasks"]["lane"], &s["tasks"]["root"]).unwrap();
    assert!(s["tasks"]["root"].get("batch_delivery").is_none());
}

#[test]
fn held_rejects_malformed_work_state_and_missing_consumption() {
    let s = accepted();
    for field in ["pending", "operations"] {
        let mut bad = s.clone();
        bad["tasks"]["root"][field] = json!(false);
        assert!(progress::claim::held(&bad, &bad["tasks"]["source"]).is_err());
    }
    let mut s = s;
    add(&mut s, "next", root(prior()), A);
    s["tasks"]["source"]["batch_delivery"]
        .as_object_mut()
        .unwrap()
        .remove("claimed_by");
    assert!(progress::claim::validate(&s, &s["tasks"]["next"]).is_err());
}

#[test]
fn cancelled_source_still_occupies_pending_and_legacy_still_reads() {
    let mut s = accepted();
    progress::control(
        &mut s,
        Some(&json!("source")),
        Some(&json!(1)),
        Some(&json!("cancelled")),
        Some(&json!("operator")),
        2.0,
    )
    .unwrap();
    assert_eq!(
        s["tasks"]["lane"]["lane"]["pending"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    progress::release(
        &mut s,
        &json!({"task":"source","state_version":2,"evidence":"stopped"}),
        3.0,
    )
    .unwrap();
    assert!(s["tasks"]["source"]["batch_delivery"]
        .get("receipt_id")
        .is_some());
    assert!(progress::task(&s, Some(&json!("lane")), Some(&json!(1))).is_ok());
}

#[test]
fn concurrent_claims_under_existing_queue_lock_have_one_winner() {
    let tmp = common::tempdir().unwrap();
    let dir = tmp.path().join("queue");
    let mut s = accepted();
    let a = spec(&mut s, "next-a", root(prior()), A);
    let b = spec(&mut s, "next-b", root(prior()), B);
    queue::save(&dir, &mut s).unwrap();
    let jobs: Vec<_> = [a, b]
        .into_iter()
        .map(|spec| {
            let dir = dir.clone();
            std::thread::spawn(move || {
                let _lock = queue::locked(&dir, true).unwrap();
                let mut s = queue::load_queue(&dir).unwrap();
                if progress::register(&mut s, &spec, 2.0).is_err() {
                    return false;
                }
                queue::save(&dir, &mut s).unwrap();
                true
            })
        })
        .collect();
    assert_eq!(
        jobs.into_iter()
            .map(|j| usize::from(j.join().unwrap()))
            .sum::<usize>(),
        1
    );
    let s = queue::load_queue(&dir).unwrap();
    let name = s["tasks"]["source"]["batch_delivery"]["claimed_by"]["task"]
        .as_str()
        .unwrap();
    progress::claim::validate(&s, &s["tasks"][name]).unwrap();
}
