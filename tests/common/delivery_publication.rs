use super::*;
use herdr_flow::{ci::GitHubApi, delivery::publication, pr};
use std::cell::{Cell, RefCell};

struct Remote {
    queue: PathBuf,
    base: String,
    landed: Cell<bool>,
    pr: RefCell<J>,
    calls: Cell<usize>,
    mutation: Cell<u8>,
    fail_branch: Cell<bool>,
}

impl GitHubApi for Remote {
    fn get(&self, suffix: &str) -> herdr_flow::Result<J> {
        // Every network observation must leave the shared transaction available.
        let _lock = queue::locked(&self.queue, false)?;
        self.calls.set(self.calls.get() + 1);
        let mutation = self.mutation.replace(0);
        if mutation != 0 {
            let mut state = queue::load_queue(&self.queue)?;
            if mutation == 1 {
                state["tasks"]["other"]["progress"] = json!("independent progress");
            } else {
                state["tasks"]["job"]["state_version"] = json!(999);
            }
            queue::save(&self.queue, &mut state)?;
        }
        if suffix == "branches/main" {
            if self.fail_branch.get() {
                return Err(herdr_flow::FlowError::progress(
                    "target branch observation unavailable",
                ));
            }
            return Ok(
                json!({"commit":{"sha":if self.landed.get() { "c".repeat(40) } else { self.base.clone() }}}),
            );
        }
        if suffix.starts_with("compare/") {
            return Ok(json!({"status":"identical","merge_base_commit":{"sha":"c".repeat(40)}}));
        }
        if suffix == "pulls/7" {
            return Ok(self.pr.borrow().clone());
        }
        if suffix.starts_with("pulls?") {
            return Ok(json!([self.pr.borrow().clone()]));
        }
        panic!("unexpected remote observation: {suffix}");
    }
}

fn fixture() -> (Fixture, J, Remote) {
    let mut f = Fixture::new(true);
    git_cmd(
        &f.env.workspace_root,
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/owner/repo.git",
        ],
    );
    let mut s = queue::load_queue(&f.queue).unwrap();
    let req = &mut s["tasks"]["job"]["requirements"]["1"];
    req["basis"]["endpoint"] = json!("https://github.com/owner/repo/tree/main");
    req["authorization"]["publication"] =
        json!({"action":"publication","source":"approved operator policy"});
    req["authorization"]["merge"] = json!({"action":"merge","source":"approved operator policy"});
    queue::save(&f.queue, &mut s).unwrap();
    let a = artifact(&f);
    begin(&f, "remote-stage");
    approve(&mut f, "remote-review", &a);
    let request = json!({"id":"remote-one","state_version":f.spec["state_version"],
        "authorization":"publish","publication_authorization":"publication","merge_authorization":"merge",
        "original_base":f.spec["expected"],"issue":130,"title":"Reviewed change","body":"Closes #130",
        "policy":{"host":"github.com","repo":"owner/repo","base_ref":"main",
        "endpoint":"https://github.com/owner/repo/tree/main","merge_method":"merge",
        "workflows":[{"file":"CI.yml","event":"pull_request","decision":"required_jobs","jobs":["test"]}],
        "landing_workflows":[{"file":"CI.yml","event":"push"}]}});
    let api = Remote {
        queue: f.queue.clone(),
        base: f.spec["expected"].as_str().unwrap().to_owned(),
        landed: Cell::new(false),
        pr: RefCell::new(J::Null),
        calls: Cell::new(0),
        mutation: Cell::new(0),
        fail_branch: Cell::new(false),
    };
    (f, request, api)
}

#[test]
fn preparation_preserves_foreign_progress_and_rejects_changed_parent_after_remote_read() {
    for mutation in [1, 2] {
        let (mut f, request, api) = fixture();
        api.mutation.set(mutation);
        let outcome = publication::prepare_remote(&mut f.env, &f.queue, &request, &api);
        let after = queue::load_queue(&f.queue).unwrap();
        if mutation == 1 {
            outcome.unwrap();
            assert_eq!(after["tasks"]["other"]["progress"], "independent progress");
            assert!(after["tasks"]["publish-remote-one"]["pr"].is_object());
        } else {
            assert!(outcome.is_err());
            assert_eq!(after["tasks"]["job"]["state_version"], 999);
            assert!(after["tasks"]["publish-remote-one"].is_null());
            assert!(after["waits"]["publish-remote-one"].is_null());
            assert!(f.env.delivery.is_null());
        }
        destroy_environment(&f.env).unwrap();
    }
}

fn prepare(f: &mut Fixture, request: &J, api: &Remote) -> J {
    publication::prepare_remote(&mut f.env, &f.queue, request, api).unwrap();
    queue::load_queue(&f.queue).unwrap()
}

fn applied_result(s: &mut J, name: &str) {
    let w = s["waits"][name].clone();
    let mut event = json!({"id":"publication-result","task":name,"wait":name,"acknowledged":true,"outcome":"complete"});
    progress::attach(s, &mut event, &w, 5.0).unwrap();
    s["events"]["publication-result"] = event;
    let spec = json!({"task":name,"state_version":s["tasks"][name]["state_version"],
        "event":"publication-result","next_action":{"kind":"blocked","owner":"host",
        "reason":"host inspected actual remote state","condition":"continue confirmed stage"}});
    progress::apply(s, &spec, 6.0).unwrap();
}

fn landing(s: &mut J, name: &str, api: &Remote) {
    let record = &s["tasks"][name]["pr"];
    *api.pr.borrow_mut() = json!({"number":7,"state":"closed","merged":true,"merge_commit_sha":"c".repeat(40),
        "head":{"sha":record["head"],"ref":record["head_ref"],"repo":{"full_name":"owner/repo"}},
        "base":{"sha":record["target_base"],"ref":"main","repo":{"full_name":"owner/repo"}}});
    api.landed.set(true);
    let spec = json!({"task":name,"state_version":s["tasks"][name]["state_version"]});
    pr::reconcile(s, &spec, 4.0, Some(api)).unwrap();
    assert_eq!(s["tasks"][name]["pr"]["phase"], "merged");
}

#[test]
fn frozen_publication_applies_without_changing_parent_version_and_confirmed_stage_continues() {
    let (mut f, request, api) = fixture();
    let mut s = prepare(&mut f, &request, &api);
    let name = "publish-remote-one";
    let frozen = s["tasks"]["job"].clone();
    assert!(lineage::action(
        &mut s,
        &json!({"kind":"activity","task":"job",
        "state_version":frozen["state_version"],"activity":"forged","role":"publication"}),
        4.0
    )
    .is_err());
    assert!(lineage::action(
        &mut s,
        &json!({"kind":"artifact","task":"job",
        "state_version":frozen["state_version"]}),
        4.0
    )
    .is_err());
    assert!(lineage::actionable(&s, &s["tasks"][name]));
    assert!(s["waits"][name]["deadline"].is_null());
    let mut long_wait = s.clone();
    long_wait["waits"] = json!({name:s["waits"][name]});
    queue::collect(&f.queue, &mut long_wait, util::now() + 10_000_000.0).unwrap();
    assert_eq!(long_wait["waits"][name]["closed"], false);
    assert_eq!(long_wait["waits"][name]["outcome"], "pending");
    let mut begin = json!({"task":name,"state_version":s["tasks"][name]["state_version"],"authorization":"missing"});
    assert!(pr::publication_begin_with_api(&mut s, &begin, 4.0, &api).is_err());
    begin["authorization"] = json!("publication");
    pr::publication_begin_with_api(&mut s, &begin, 4.0, &api).unwrap();
    publication::revalidate_slot(&s, &s["tasks"][name]).unwrap();
    begin["state_version"] = s["tasks"][name]["state_version"].clone();
    assert!(pr::publication_begin_with_api(&mut s, &begin, 4.0, &api).is_err());
    queue::save(&f.queue, &mut s).unwrap();
    publication::reserve_remote(&mut f.env).unwrap();
    let reserved = f.env.delivery.clone();
    let reserved_queue = queue::load_queue(&f.queue).unwrap();
    assert_eq!(publication::reserve_remote(&mut f.env).unwrap(), reserved);
    assert_eq!(queue::load_queue(&f.queue).unwrap(), reserved_queue);
    assert_eq!(
        publication::confirm_remote(&mut f.env, &f.queue, &api).unwrap()["phase"],
        "unknown"
    );
    landing(&mut s, name, &api);
    queue::save(&f.queue, &mut s).unwrap();
    let exact_pr = api.pr.borrow().clone();
    api.pr.borrow_mut()["head"]["sha"] = json!("d".repeat(40));
    assert_eq!(
        publication::confirm_remote(&mut f.env, &f.queue, &api).unwrap()["phase"],
        "unknown"
    );
    *api.pr.borrow_mut() = exact_pr;
    assert_eq!(
        publication::confirm_remote(&mut f.env, &f.queue, &api).unwrap()["phase"],
        "confirmed"
    );
    let confirmed_calls = api.calls.get();
    publication::confirm_remote(&mut f.env, &f.queue, &api).unwrap();
    assert_eq!(api.calls.get(), confirmed_calls);
    assert!(
        delivery::complete_stage(&mut f.env, &f.queue, "remote-one").is_err(),
        "an actual landing still needs the original applied publisher result"
    );
    s = queue::load_queue(&f.queue).unwrap();
    applied_result(&mut s, name);
    assert_eq!(s["tasks"]["job"], frozen);
    queue::save(&f.queue, &mut s).unwrap();
    assert_eq!(
        delivery::complete_stage(&mut f.env, &f.queue, "remote-one").unwrap()["phase"],
        "complete"
    );
    let resumed = queue::load_queue(&f.queue).unwrap();
    assert_eq!(resumed["tasks"]["job"]["control"], "running");
    assert!(resumed["tasks"]["job"]["publication_preparation"].is_null());
    destroy_environment(&f.env).unwrap();
}

#[test]
fn exact_preparation_recovers_both_persisted_prefixes_without_new_child_or_remote_reads() {
    let (mut f, request, api) = fixture();
    let prepared = prepare(&mut f, &request, &api);
    let journal = f.env.delivery.clone();
    let calls = api.calls.get();
    let mut prefix = prepared.clone();
    prefix["tasks"]["job"]["control"] = json!("running");
    prefix["tasks"]["job"]["delivery"] = J::Null;
    for persisted_journal in [false, true] {
        queue::save(&f.queue, &mut prefix).unwrap();
        f.env.delivery = if persisted_journal {
            journal.clone()
        } else {
            J::Null
        };
        f.env.persist().unwrap();
        let restored = prepare(&mut f, &request, &api);
        assert_eq!(restored["tasks"], prepared["tasks"]);
        assert_eq!(f.env.delivery, journal);
        assert_eq!(api.calls.get(), calls);
    }
    publication::reserve_remote(&mut f.env).unwrap();
    let unknown = f.env.delivery.clone();
    queue::save(&f.queue, &mut prefix).unwrap();
    assert_eq!(publication::reserve_remote(&mut f.env).unwrap(), unknown);
    assert_eq!(
        queue::load_queue(&f.queue).unwrap()["tasks"]["job"]["control"],
        "delivering"
    );
    assert_eq!(api.calls.get(), calls);
    let mut changed = request.clone();
    changed["title"] = json!("different request");
    assert!(publication::prepare_remote(&mut f.env, &f.queue, &changed, &api).is_err());
    destroy_environment(&f.env).unwrap();
}

#[test]
fn publisher_holds_merged_target_until_host_receipt_and_repairs_missing_marker_without_requery() {
    let (mut f, request, api) = fixture();
    let mut state = prepare(&mut f, &request, &api);
    let name = "publish-remote-one";
    let mut successor = state["tasks"][name].clone();
    successor["task"] = json!("successor");
    successor.as_object_mut().unwrap().remove("lineage");
    successor["pr"]["head_ref"] = json!("next-publication");
    assert!(
        publication::revalidate_slot(&state, &successor).is_ok(),
        "unstarted intent has no slot"
    );
    let spec = json!({"task":name,"state_version":state["tasks"][name]["state_version"],"authorization":"publication"});
    pr::publication_begin_with_api(&mut state, &spec, 4.0, &api).unwrap();
    assert!(publication::revalidate_slot(&state, &successor).is_err());
    queue::save(&f.queue, &mut state).unwrap();
    publication::reserve_remote(&mut f.env).unwrap();
    landing(&mut state, name, &api);
    assert!(
        publication::revalidate_slot(&state, &successor).is_err(),
        "postmerge CI still owns this target"
    );
    queue::save(&f.queue, &mut state).unwrap();
    let mut legacy = state.clone();
    legacy["tasks"][name]
        .as_object_mut()
        .unwrap()
        .remove("lineage");
    assert!(
        publication::revalidate_slot(&legacy, &successor).is_ok(),
        "ordinary PR release behavior remains unchanged"
    );
    api.fail_branch.set(true);
    let before_failed_verification = api.calls.get();
    assert_eq!(
        publication::confirm_remote(&mut f.env, &f.queue, &api).unwrap()["phase"],
        "unknown"
    );
    assert_eq!(
        api.calls.get(),
        before_failed_verification + 2,
        "merged PR GET succeeded; current target GET failed"
    );
    let unconfirmed = queue::load_queue(&f.queue).unwrap();
    assert!(unconfirmed["tasks"][name]["publication_delivery"].is_null());
    assert!(publication::revalidate_slot(&unconfirmed, &successor).is_err());
    api.fail_branch.set(false);
    publication::confirm_remote(&mut f.env, &f.queue, &api).unwrap();
    let confirmed = queue::load_queue(&f.queue).unwrap();
    assert!(publication::revalidate_slot(&confirmed, &successor).is_ok());
    assert_eq!(confirmed["tasks"]["job"], state["tasks"]["job"]);
    let marker = confirmed["tasks"][name]["publication_delivery"].clone();
    let mut interrupted = confirmed.clone();
    interrupted["tasks"][name]
        .as_object_mut()
        .unwrap()
        .remove("publication_delivery");
    queue::save(&f.queue, &mut interrupted).unwrap();
    assert!(publication::revalidate_slot(&interrupted, &successor).is_err());
    let calls = api.calls.get();
    publication::confirm_remote(&mut f.env, &f.queue, &api).unwrap();
    assert_eq!(api.calls.get(), calls);
    let repaired = queue::load_queue(&f.queue).unwrap();
    assert_eq!(repaired["tasks"][name]["publication_delivery"], marker);
    assert!(publication::revalidate_slot(&repaired, &successor).is_ok());
    for key in ["head", "commit", "repo", "base_ref", "number"] {
        let mut wrong = repaired.clone();
        wrong["tasks"][name]["publication_delivery"]["receipt"][key] = J::Null;
        assert!(
            publication::revalidate_slot(&wrong, &successor).is_err(),
            "{key}"
        );
    }
    for key in ["id", "task", "requirement", "phase"] {
        let mut wrong = repaired.clone();
        wrong["tasks"][name]["publication_delivery"][key] = J::Null;
        assert!(
            publication::revalidate_slot(&wrong, &successor).is_err(),
            "{key}"
        );
    }
    destroy_environment(&f.env).unwrap();
}

#[test]
fn preparation_recovery_rejects_changed_inputs_and_cancelled_parents_without_mutation() {
    let (mut f, request, api) = fixture();
    let mut state = prepare(&mut f, &request, &api);
    let calls = api.calls.get();
    for key in [
        "id",
        "state_version",
        "original_base",
        "issue",
        "authorization",
        "publication_authorization",
        "merge_authorization",
        "title",
        "body",
        "policy",
    ] {
        let mut changed = request.clone();
        changed[key] = J::Null;
        assert!(
            publication::prepare_remote(&mut f.env, &f.queue, &changed, &api).is_err(),
            "{key}"
        );
        assert_eq!(queue::load_queue(&f.queue).unwrap(), state);
    }
    state["tasks"]["job"]["control"] = json!("cancelled");
    queue::save(&f.queue, &mut state).unwrap();
    let journal = f.env.delivery.clone();
    assert!(publication::prepare_remote(&mut f.env, &f.queue, &request, &api).is_err());
    assert!(publication::reserve_remote(&mut f.env).is_err());
    assert!(publication::confirm_remote(&mut f.env, &f.queue, &api).is_err());
    assert_eq!(f.env.delivery, journal);
    assert_eq!(queue::load_queue(&f.queue).unwrap(), state);
    assert_eq!(api.calls.get(), calls);
    f.env.delivery = J::Null;
    f.env.persist().unwrap();
    assert!(publication::prepare_remote(&mut f.env, &f.queue, &request, &api).is_err());
    assert!(f.env.delivery.is_null());
    assert_eq!(queue::load_queue(&f.queue).unwrap(), state);
    destroy_environment(&f.env).unwrap();
}

#[test]
fn malformed_premerge_and_landing_ci_policies_fail_before_child_journal_or_api() {
    let (mut f, request, api) = fixture();
    let original = queue::load_queue(&f.queue).unwrap();
    let journal = util::read_json(f.env.state_file()).unwrap();
    let malformed = [
        J::Null,
        json!("CI.yml"),
        json!(["CI.yml"]),
        json!([{"workflow":"CI.yml","event":"pull_request"}]),
        json!([{"file":"../CI.yml","event":"pull_request"}]),
        json!([{"file":"CI.yml","event":"push&x=1"}]),
        json!([{"file":"CI.yml","event":"push","decision":"unknown"}]),
        json!([{"file":"CI.yml","event":"push","decision":"required_jobs","jobs":[]}]),
        json!([{"file":"CI.yml","event":"push","decision":"required_jobs","jobs":["test","test"]}]),
        json!([{"file":"CI.yml","event":"push"},{"file":"CI.yml","event":"push"}]),
    ];
    for key in ["workflows", "landing_workflows"] {
        for inventory in &malformed {
            let mut wrong = request.clone();
            wrong["policy"][key] = inventory.clone();
            assert!(
                publication::prepare_remote(&mut f.env, &f.queue, &wrong, &api).is_err(),
                "{key}: {inventory}"
            );
            assert_eq!(queue::load_queue(&f.queue).unwrap(), original);
            assert_eq!(util::read_json(f.env.state_file()).unwrap(), journal);
            assert!(f.env.delivery.is_null());
            assert_eq!(api.calls.get(), 0);
        }
    }
    let mut empty = request.clone();
    empty["policy"]["workflows"] = json!([]);
    assert!(publication::prepare_remote(&mut f.env, &f.queue, &empty, &api).is_err());
    assert_eq!(api.calls.get(), 0);
    assert_eq!(queue::load_queue(&f.queue).unwrap(), original);
    let mut no_landing_inventory = request;
    no_landing_inventory["policy"]["landing_workflows"] = json!([]);
    prepare(&mut f, &no_landing_inventory, &api);
    destroy_environment(&f.env).unwrap();
}

#[test]
fn premerge_whole_workflow_policies_are_rejected_before_mutation_or_api() {
    let (mut f, request, api) = fixture();
    let original = queue::load_queue(&f.queue).unwrap();
    let journal = util::read_json(f.env.state_file()).unwrap();
    for row in [
        json!({"file":"CI.yml","event":"pull_request"}),
        json!({"file":"CI.yml","event":"pull_request","decision":"workflow"}),
        json!({"file":"CI.yml","event":"pull_request","decision":"workflow","jobs":["test"]}),
    ] {
        let mut incompatible = request.clone();
        incompatible["policy"]["workflows"] = json!([row]);
        let error =
            publication::prepare_remote(&mut f.env, &f.queue, &incompatible, &api).unwrap_err();
        assert!(error
            .message
            .contains("premerge workflows require decision=required_jobs"));
        assert_eq!(queue::load_queue(&f.queue).unwrap(), original);
        assert_eq!(util::read_json(f.env.state_file()).unwrap(), journal);
        assert!(f.env.delivery.is_null());
        assert_eq!(api.calls.get(), 0);
    }
    // The same general workflow decision remains supported for landing CI.
    prepare(&mut f, &request, &api);
    destroy_environment(&f.env).unwrap();
}

#[test]
fn publication_rejects_wrong_target_authority_and_each_stale_acceptance_binding() {
    let (mut f, request, api) = fixture();
    let original = queue::load_queue(&f.queue).unwrap();
    let mut model_request = original.clone();
    let error = lineage::action(
        &mut model_request,
        &json!({"kind":"activity","task":"job",
        "state_version":request["state_version"],"activity":"forged","role":"publication"}),
        4.0,
    )
    .unwrap_err();
    assert!(error.message.contains("invalid activity role"));
    assert_eq!(model_request, original);
    for (key, value) in [
        ("endpoint", json!("https://github.com/other/repo/tree/main")),
        ("repo", json!("other/repo")),
        ("merge_method", json!("squash")),
        ("command", json!("unsafe")),
    ] {
        let mut wrong = request.clone();
        wrong["policy"][key] = value;
        assert!(
            publication::prepare_remote(&mut f.env, &f.queue, &wrong, &api).is_err(),
            "{key}"
        );
        assert_eq!(queue::load_queue(&f.queue).unwrap(), original);
    }
    for key in [
        "authorization",
        "publication_authorization",
        "merge_authorization",
    ] {
        let mut wrong = request.clone();
        wrong[key] = json!("missing");
        assert!(publication::prepare_remote(&mut f.env, &f.queue, &wrong, &api).is_err());
    }
    let prepared = prepare(&mut f, &request, &api);
    for key in [
        "candidate",
        "acceptance_round",
        "requirement_revision",
        "scope_epoch",
        "approved",
        "delivery",
    ] {
        let mut stale = prepared.clone();
        stale["tasks"]["job"][key] = J::Null;
        assert!(
            !lineage::actionable(&stale, &stale["tasks"]["publish-remote-one"]),
            "{key}"
        );
    }
    destroy_environment(&f.env).unwrap();
}
