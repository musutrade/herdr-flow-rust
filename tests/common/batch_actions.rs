use super::*;
use std::cell::Cell;
use std::collections::BTreeMap;

#[derive(Default, Clone)]
struct Api(BTreeMap<String, J>);
impl GitHubApi for Api {
    fn get(&self, path: &str) -> Result<J> {
        self.0
            .get(path)
            .cloned()
            .ok_or_else(|| crate::error::FlowError::watch(format!("unexpected GET {path}")))
    }
}
fn policy_value() -> J {
    json!({"kind":"github-actions","version":1,
        "workflows":[{"file":"rust.yml","event":"pull_request","decision":"required_jobs","jobs":["test","msrv"]}],
        "landing_workflows":[{"file":"rust.yml","event":"push","decision":"required_jobs","jobs":["test","msrv"]}]})
}
fn fixture() -> (J, J, J, Api) {
    let ci = policy_value();
    let lane = json!({"repo":"o/r","host":"github.com","base_ref":"main","ci_policy_digest":digest(&ci),"ci":ci,"gate":null});
    let batch = json!({"id":"b1","revision":1,"phase":"snapshot_frozen","base":"b".repeat(40),"snapshot":{"head":"a".repeat(40),"tree":"c".repeat(40)},"gate":null});
    let t = json!({"task":"lane","epoch":2,"pr":{"phase":"registered","number":7,"head":batch["snapshot"]["head"],"target_base":batch["base"],"host":"github.com","repo":"o/r","base_ref":"main","head_ref":"batch/b1"}});
    let mut api = Api::default();
    api.0.insert(
        "pulls/7".into(),
        json!({"number":7,"state":"open","merged":false,
        "head":{"sha":batch["snapshot"]["head"],"ref":"batch/b1","repo":{"full_name":"o/r"}},
        "base":{"sha":batch["base"],"ref":"main","repo":{"full_name":"o/r"}}}),
    );
    api.0.insert(
        "branches/main".into(),
        json!({"commit":{"sha":batch["base"]}}),
    );
    install_run(
        &mut api,
        &batch["snapshot"]["head"],
        "pull_request",
        10,
        1,
        "success",
    );
    (t, lane, batch, api)
}
fn install_run(api: &mut Api, sha: &J, event: &str, id: i64, attempt: i64, conclusion: &str) {
    let run = json!({"id":id,"head_sha":sha,"run_attempt":attempt,"status":"completed","conclusion":conclusion,"event":event,"path":".github/workflows/rust.yml","head_branch":if event == "push" {"main"} else {"batch/b1"},"head_repository":{"full_name":"o/r"},"pull_requests":[{"number":7}]});
    let jobs:Vec<J>=["test","msrv"].iter().enumerate().map(|(i,name)|json!({"id":id*100+i as i64,"run_id":id,"head_sha":sha,"name":name,"status":"completed","conclusion":conclusion})).collect();
    api.0.insert(
        format!(
            "actions/workflows/rust.yml/runs?head_sha={}&event={event}&per_page=100&page=1",
            sha.as_str().unwrap()
        ),
        json!({"workflow_runs":[run.clone()]}),
    );
    api.0.insert(
        format!("actions/runs/{id}/attempts/{attempt}/jobs?per_page=100&page=1"),
        json!({"jobs":jobs}),
    );
    api.0.insert(format!("actions/runs/{id}"), run);
}
fn failure(t: &J, lane: &J, batch: &mut J, api: &mut Api) {
    install_run(
        api,
        &batch["snapshot"]["head"],
        "pull_request",
        10,
        1,
        "failure",
    );
    batch["ci"] = observe(t, lane, batch, 100., api).unwrap();
    batch["phase"] = json!("ci_failed");
    batch["failure_token"] = json!(repair_token(batch));
}
#[test]
fn explicit_policy_is_strict_exclusive_and_pinned() {
    let (_, lane, _, _) = fixture();
    assert!(check(&lane).is_ok());
    assert_eq!(
        initialize(&json!({"ci":policy_value()})).unwrap(),
        Some(policy_value())
    );
    assert_eq!(initialize(&json!({"gate":{}})).unwrap(), None);
    assert!(check(&json!({"gate":{}})).is_ok());
    for value in [
        json!(null),
        json!({}),
        json!({"kind":"github-actions","version":2}),
    ] {
        assert!(initialize(&json!({"ci":value})).is_err());
    }
    assert!(initialize(&json!({"ci":policy_value(),"gate":null})).is_err());
    for (path, value) in [
        ("/kind", json!("gate")),
        ("/version", json!(2)),
        ("/workflows", json!([])),
        ("/landing_workflows", json!([])),
        ("/workflows/0/event", json!("push")),
        ("/landing_workflows/0/event", json!("pull_request")),
        ("/workflows/0/decision", json!("run")),
        ("/workflows/0/jobs", json!(["test", "test"])),
        ("/workflows/0/jobs", json!([""])),
        ("/workflows/0/jobs", json!([])),
        ("/workflows/0/file", json!("../bad")),
    ] {
        let mut p = policy_value();
        *p.pointer_mut(path).unwrap() = value;
        assert!(initialize(&json!({"ci":p})).is_err(), "{path}");
    }
    for path in ["", "/workflows/0", "/landing_workflows/0"] {
        let mut p = policy_value();
        p.pointer_mut(path).unwrap()["app_id"] = json!(1);
        assert!(initialize(&json!({"ci":p})).is_err());
    }
    let mut changed = lane.clone();
    changed["ci"]["workflows"][0]["jobs"] = json!(["test"]);
    assert!(check(&changed).is_err());
    changed = lane.clone();
    changed["gate"] = json!({});
    assert!(check(&changed).is_err());
}
#[test]
fn publication_requires_snapshot_and_no_gate() {
    let (_, lane, mut batch, _) = fixture();
    assert!(publication(&lane, &batch).is_ok());
    for phase in ["ci_passed", "ci_failed", "integrating", "gate_passed"] {
        batch["phase"] = json!(phase);
        assert!(publication(&lane, &batch).is_err());
    }
    batch["phase"] = json!("snapshot_frozen");
    batch["gate"] = json!({});
    assert!(publication(&lane, &batch).is_err());
}
#[test]
fn observation_requires_registered_pr_and_policy_identity() {
    let (t, lane, batch, api) = fixture();
    let result = observe(&t, &lane, &batch, 100., &api).unwrap();
    assert_eq!(result["state"], "success");
    assert_eq!(result["pr"]["task"], "lane");
    assert_eq!(result["pr"]["epoch"], 2);
    assert_eq!(result["policy_digest"], lane["ci_policy_digest"]);
    for (path, value) in [
        ("/pr/number", json!(null)),
        ("/pr/phase", json!("sending")),
        ("/pr/head", json!("wrong")),
        ("/pr/target_base", json!("wrong")),
        ("/pr/repo", json!("x/y")),
        ("/pr/base_ref", json!("other")),
    ] {
        let mut bad = t.clone();
        *bad.pointer_mut(path).unwrap() = value;
        assert!(observe(&bad, &lane, &batch, 100., &api).is_err(), "{path}");
    }
    for value in [json!(null), json!("0"), json!(0.5)] {
        let mut bad = batch.clone();
        bad["ci_observations"] = value;
        assert!(observe(&t, &lane, &bad, 100., &api).is_err());
    }
    for path in ["/base", "/snapshot/head", "/snapshot/tree"] {
        let mut bad = batch.clone();
        *bad.pointer_mut(path).unwrap() = json!(null);
        assert!(observe(&t, &lane, &bad, 100., &api).is_err());
    }
    for count in [-1, 256, 257] {
        let mut bad = batch.clone();
        bad["ci_observations"] = json!(count);
        assert!(observe(&t, &lane, &bad, 100., &api).is_err());
    }
    for phase in ["gate_failed", "repair_update_pending", "integrated"] {
        let mut bad = batch.clone();
        bad["phase"] = json!(phase);
        assert!(observe(&t, &lane, &bad, 100., &api).is_err());
    }
}
#[test]
fn pr_identity_and_current_base_fail_closed() {
    let (t, lane, batch, api) = fixture();
    for (path, value) in [
        ("/number", json!(8)),
        ("/head/sha", json!("d".repeat(40))),
        ("/head/ref", json!("foreign")),
        ("/head/repo/full_name", json!("evil/fork")),
        ("/base/repo/full_name", json!("x/y")),
        ("/base/ref", json!("next")),
        ("/base/sha", json!("d".repeat(40))),
        ("/merged", json!(true)),
        ("/state", json!("closed")),
    ] {
        let mut bad = api.clone();
        *bad.0.get_mut("pulls/7").unwrap().pointer_mut(path).unwrap() = value;
        assert!(observe(&t, &lane, &batch, 100., &bad).is_err(), "{path}");
    }
    let mut bad = api.clone();
    bad.0.get_mut("branches/main").unwrap()["commit"]["sha"] = json!("d".repeat(40));
    assert!(observe(&t, &lane, &batch, 100., &bad).is_err());
}
#[test]
fn uses_required_job_decision_not_advisory_run_conclusion() {
    let (t, lane, batch, mut api) = fixture();
    api.0.get_mut("actions/runs/10").unwrap()["conclusion"] = json!("failure");
    api.0.get_mut("actions/runs/10/attempts/1/jobs?per_page=100&page=1").unwrap()["jobs"].as_array_mut().unwrap().push(json!({"id":1002,"run_id":10,"name":"advisory","status":"completed","conclusion":"failure"}));
    let value = observe(&t, &lane, &batch, 100., &api).unwrap();
    assert_eq!(value["state"], "success");
    assert_eq!(value["workflows"][0]["jobs"].as_array().unwrap().len(), 2);
    api.0.get_mut("actions/runs/10").unwrap()["status"] = json!("in_progress");
    assert_eq!(
        observe(&t, &lane, &batch, 100., &api).unwrap()["state"],
        "pending"
    );
}
#[test]
fn missing_cancelled_skipped_and_neutral_do_not_authorize_repair() {
    let (t, lane, batch, api) = fixture();
    for conclusion in ["cancelled", "skipped", "neutral", "timed_out"] {
        let mut api = api.clone();
        install_run(
            &mut api,
            &batch["snapshot"]["head"],
            "pull_request",
            10,
            1,
            conclusion,
        );
        let mut failed = batch.clone();
        failed["ci"] = observe(&t, &lane, &batch, 100., &api).unwrap();
        assert_eq!(failed["ci"]["state"], "needs_attention");
        failed["failure_token"] = json!(repair_token(&failed));
        assert!(check_failure(&t, &lane, &failed, &api).is_err());
    }
    let mut api = api.clone();
    api.0
        .get_mut("actions/runs/10/attempts/1/jobs?per_page=100&page=1")
        .unwrap()["jobs"] = json!([]);
    assert_eq!(
        observe(&t, &lane, &batch, 100., &api).unwrap()["state"],
        "needs_attention"
    );
    for value in api.0.values_mut() {
        if value.get("workflow_runs").is_some() {
            value["workflow_runs"] = json!([]);
        }
    }
    assert_eq!(
        observe(&t, &lane, &batch, 100., &api).unwrap()["state"],
        "pending"
    );
}
#[test]
fn malformed_or_superseded_run_and_job_identity_is_rejected() {
    let (t, lane, batch, api) = fixture();
    for (key, value) in [
        ("id", json!(11)),
        ("run_attempt", json!(2)),
        ("head_sha", json!("f".repeat(40))),
        ("event", json!("push")),
        ("path", json!(".github/workflows/other.yml")),
    ] {
        let mut bad = api.clone();
        bad.0.get_mut("actions/runs/10").unwrap()[key] = value;
        assert!(observe(&t, &lane, &batch, 100., &bad).is_err(), "{key}");
    }
    for key in ["run_id", "id"] {
        let mut bad = api.clone();
        bad.0
            .get_mut("actions/runs/10/attempts/1/jobs?per_page=100&page=1")
            .unwrap()["jobs"][0][key] = json!(0);
        assert!(observe(&t, &lane, &batch, 100., &bad).is_err());
    }
    let mut bad = api.clone();
    let jobs = bad
        .0
        .get_mut("actions/runs/10/attempts/1/jobs?per_page=100&page=1")
        .unwrap()["jobs"]
        .as_array_mut()
        .unwrap();
    jobs.push(jobs[0].clone());
    assert!(observe(&t, &lane, &batch, 100., &bad).is_err());
}
struct Changing {
    api: Api,
    path: String,
    after: usize,
    count: Cell<usize>,
}
impl GitHubApi for Changing {
    fn get(&self, path: &str) -> Result<J> {
        let mut result = self.api.get(path)?;
        if path == self.path {
            let n = self.count.get() + 1;
            self.count.set(n);
            if n > self.after {
                if path.starts_with("pulls/") {
                    result["head"]["sha"] = json!("e".repeat(40));
                } else {
                    result["conclusion"] = json!("failure");
                }
            }
        }
        Ok(result)
    }
}
#[test]
fn rechecks_pr_after_observation_and_rejects_changing_ci() {
    let (t, lane, batch, api) = fixture();
    for (path, after) in [("pulls/7", 1), ("actions/runs/10", 2)] {
        let changing = Changing {
            api: api.clone(),
            path: path.into(),
            after,
            count: Cell::new(0),
        };
        assert!(
            observe(&t, &lane, &batch, 100., &changing).is_err(),
            "{path}"
        );
    }
}
#[test]
fn failure_token_binds_full_observation_and_live_attempt() {
    let (t, lane, mut batch, mut api) = fixture();
    failure(&t, &lane, &mut batch, &mut api);
    assert!(check_intent(&t, &lane, &batch, 110.).is_ok());
    assert!(check_failure(&t, &lane, &batch, &api).is_ok());
    assert!(check_intent(&t, &lane, &batch, 221.).is_err());
    assert!(check_intent(&t, &lane, &batch, 99.).is_err());
    for path in [
        "/ci/observed_at",
        "/ci/pr/number",
        "/ci/binding/base",
        "/ci/policy_digest",
        "/ci/workflows/0/attempt",
        "/ci/workflows/0/run_id",
        "/ci/workflows/0/jobs/0/id",
    ] {
        let mut bad = batch.clone();
        *bad.pointer_mut(path).unwrap() = json!(999);
        assert_ne!(repair_token(&bad), repair_token(&batch));
        assert!(check_failure(&t, &lane, &bad, &api).is_err(), "{path}");
    }
    let previous_token = batch["failure_token"].clone();
    install_run(
        &mut api,
        &batch["snapshot"]["head"],
        "pull_request",
        10,
        2,
        "failure",
    );
    assert!(check_failure(&t, &lane, &batch, &api).is_err());
    batch["ci"] = observe(&t, &lane, &batch, 111., &api).unwrap();
    batch["failure_token"] = json!(repair_token(&batch));
    assert_ne!(previous_token, batch["failure_token"]);
    assert!(check_failure(&t, &lane, &batch, &api).is_ok());
    install_run(
        &mut api,
        &batch["snapshot"]["head"],
        "pull_request",
        11,
        1,
        "failure",
    );
    assert!(check_failure(&t, &lane, &batch, &api).is_err());
}
#[test]
fn landing_uses_recorded_base_and_requires_both_ci_heads() {
    let (t, lane, mut batch, mut api) = fixture();
    batch["ci"] = observe(&t, &lane, &batch, 100., &api).unwrap();
    let commit = json!("d".repeat(40));
    let landed = json!({"commit":commit});
    api.0.insert(
        format!(
            "compare/{}...{}",
            batch["base"].as_str().unwrap(),
            commit.as_str().unwrap()
        ),
        json!({"status":"ahead","merge_base_commit":{"sha":batch["base"]}}),
    );
    let pr = api.0.get_mut("pulls/7").unwrap();
    pr["state"] = json!("closed");
    pr["merged"] = json!(true);
    pr["base"]["sha"] = commit.clone();
    pr["merge_commit_sha"] = commit.clone();
    api.0.get_mut("branches/main").unwrap()["commit"]["sha"] = commit.clone();
    install_run(&mut api, &commit, "push", 20, 1, "failure");
    assert!(landing(&t, &lane, &batch, &landed, &api).is_err());
    install_run(&mut api, &commit, "push", 20, 2, "success");
    assert!(landing(&t, &lane, &batch, &landed, &api).is_ok());
    let mut bad = batch.clone();
    bad["ci"]["state"] = json!("failure");
    assert!(landing(&t, &lane, &bad, &landed, &api).is_err());
    let mut wrong = t.clone();
    wrong["pr"]["number"] = json!(8);
    assert!(landing(&wrong, &lane, &batch, &landed, &api).is_err());
    api.0.get_mut("pulls/7").unwrap()["merge_commit_sha"] = json!("e".repeat(40));
    assert!(landing(&t, &lane, &batch, &landed, &api).is_err());
    install_run(
        &mut api,
        &batch["snapshot"]["head"],
        "pull_request",
        10,
        2,
        "failure",
    );
    assert!(landing(&t, &lane, &batch, &landed, &api).is_err());
}

#[test]
fn repair_begin_observation_and_stall_reuse_existing_lane_contract() {
    let (t, mut lane, mut batch, mut api) = fixture();
    failure(&t, &lane, &mut batch, &mut api);
    super::super::lane::check_repair_observation(&t, &lane, &batch, &api).unwrap();
    let original = api.0["branches/main"].clone();
    api.0.get_mut("branches/main").unwrap()["commit"]["sha"] = json!("e".repeat(40));
    assert!(super::super::lane::check_repair_observation(&t, &lane, &batch, &api).is_err());
    api.0.insert("branches/main".into(), original);
    lane["active"] = batch.clone();
    lane["workspace_root"] = json!("/isolated/integration");
    lane["max_repairs"] = json!(2);
    lane["repairs"] = json!({});
    let mut owner = t.clone();
    owner["lane"] = lane;
    owner["phase"] = json!("blocked");
    owner["candidate"] = batch["snapshot"]["head"].clone();
    let binding = json!({"batch":batch["id"],"token":batch["failure_token"],"candidate":owner["candidate"],"base":batch["base"]});
    let action = json!({"operation":"repair-one","writes":true,"workspace":"/isolated/repair","batch_repair":binding,"repair_diagnosis":{"kind":"code","evidence":"inspected required test failure"}});
    let event = json!({"outcome":"failed"});
    let unchanged = owner.clone();
    assert!(super::super::lane::repair_intent(&mut owner, &action, &event, 221.).is_err());
    assert_eq!(owner, unchanged);
    super::super::lane::repair_intent(&mut owner, &action, &event, 101.).unwrap();
    let op = json!({"intent":action});
    super::super::lane::check_repair(&json!({}), &owner, &op, 102., false).unwrap();
    let mut stale = op.clone();
    stale["intent"]["batch_repair"]["token"] = json!("old");
    assert!(super::super::lane::check_repair(&json!({}), &owner, &stale, 102., false).is_err());
    for phase in ["ci_pending", "ci_passed", "ci_failed"] {
        owner["lane"]["active"]["phase"] = json!(phase);
        owner["lane"]["active"]["progress_deadline"] = json!(100.);
        assert!(super::super::lane::stall(&owner, 200.).is_some());
    }
}

#[test]
fn ci_failure_cannot_be_reinterpreted_as_legacy_gate_repair() {
    let (t, lane, mut batch, mut api) = fixture();
    failure(&t, &lane, &mut batch, &mut api);
    assert!(check_intent(&t, &json!({"gate":{}}), &batch, 101.).is_err());
    assert!(check_failure(&t, &json!({"gate":{}}), &batch, &api).is_err());
    let mut malformed = lane.clone();
    malformed["ci_policy_digest"] = json!("changed");
    batch["phase"] = json!("integration_conflict");
    assert!(check_intent(&t, &malformed, &batch, 101.).is_err());
}

#[test]
fn run_target_and_auto_merge_are_checked_for_pr_and_push() {
    let (t, lane, batch, api) = fixture();
    for (key, value) in [
        ("head_branch", json!("other")),
        ("head_repository", json!({"full_name":"fork/r"})),
        ("pull_requests", json!([])),
        ("pull_requests", json!([{"number":8}])),
    ] {
        let mut bad = api.clone();
        bad.0.get_mut("actions/runs/10").unwrap()[key] = value;
        assert!(observe(&t, &lane, &batch, 100., &bad).is_err(), "{key}");
    }
    let mut bad = api.clone();
    bad.0.get_mut("pulls/7").unwrap()["auto_merge"] = json!({"merge_method":"squash"});
    assert!(observe(&t, &lane, &batch, 100., &bad).is_err());
    assert!(publication_settings(&t, &bad).is_err());
    assert!(publication_settings(&t, &api).is_ok());
    let pr = registered(&t, &lane, &batch).unwrap();
    let config = config(&lane, &batch["snapshot"]["head"], "landing_workflows", &pr);
    let mut api = api;
    install_run(&mut api, &config["sha"], "push", 20, 1, "success");
    assert!(observation::verified(&config, &api).is_ok());
    api.0.get_mut("actions/runs/20").unwrap()["head_branch"] = json!("feature");
    assert!(observation::verified(&config, &api).is_err());
}
#[test]
fn unfinished_failure_is_pending_and_completed_failure_is_stable() {
    let (t, lane, mut batch, mut api) = fixture();
    install_run(
        &mut api,
        &batch["snapshot"]["head"],
        "pull_request",
        10,
        1,
        "failure",
    );
    api.0.get_mut("actions/runs/10").unwrap()["status"] = json!("in_progress");
    assert_eq!(
        observe(&t, &lane, &batch, 100., &api).unwrap()["state"],
        "pending"
    );
    api.0.get_mut("actions/runs/10").unwrap()["status"] = json!("completed");
    batch["ci"] = observe(&t, &lane, &batch, 101., &api).unwrap();
    batch["phase"] = json!("ci_failed");
    batch["failure_token"] = json!(repair_token(&batch));
    assert!(check_failure(&t, &lane, &batch, &api).is_ok());
    let source = batch.clone();
    batch["phase"] = json!("integrated");
    assert_eq!(
        super::super::lane::repair_token(&batch),
        repair_token(&source)
    );
    check_repair_source(&t, &lane, &batch, &source, &api).unwrap();
    for (key, value) in [("id", json!("other")), ("revision", json!(2))] {
        let mut bad = batch.clone();
        bad[key] = value;
        assert!(check_repair_source(&t, &lane, &bad, &source, &api).is_err());
    }
    for (key, value) in [("task", json!("other")), ("epoch", json!(3))] {
        let mut wrong = t.clone();
        wrong[key] = value;
        assert!(check_failure(&wrong, &lane, &source, &api).is_err());
        assert!(check_intent(&wrong, &lane, &source, 102.).is_err());
    }
}

#[test]
fn failure_waits_for_every_workflow_terminal_decision() {
    let (t, mut lane, batch, mut api) = fixture();
    let mut second = lane["ci"]["workflows"][0].clone();
    second["file"] = json!("other.yml");
    lane["ci"]["workflows"].as_array_mut().unwrap().push(second);
    lane["ci_policy_digest"] = json!(digest(&lane["ci"]));
    let head = &batch["snapshot"]["head"];
    install_run(&mut api, head, "pull_request", 10, 1, "failure");
    let path = format!(
        "actions/workflows/other.yml/runs?head_sha={}&event=pull_request&per_page=100&page=1",
        head.as_str().unwrap()
    );
    api.0.insert(path.clone(), json!({"workflow_runs":[]}));
    assert_eq!(
        observe(&t, &lane, &batch, 100., &api).unwrap()["state"],
        "pending"
    );
    let mut second_api = Api::default();
    install_run(&mut second_api, head, "pull_request", 20, 1, "success");
    for (key, mut value) in second_api.0 {
        if let Some(runs) = value.get_mut("workflow_runs") {
            runs[0]["path"] = json!(".github/workflows/other.yml");
        } else if value.get("path").is_some() {
            value["path"] = json!(".github/workflows/other.yml");
        }
        api.0.insert(
            key.replace("workflows/rust.yml/", "workflows/other.yml/"),
            value,
        );
    }
    api.0.get_mut("actions/runs/20").unwrap()["status"] = json!("in_progress");
    assert_eq!(
        observe(&t, &lane, &batch, 101., &api).unwrap()["state"],
        "pending"
    );
    api.0.get_mut("actions/runs/20").unwrap()["status"] = json!("completed");
    let mut failed = batch.clone();
    failed["ci"] = observe(&t, &lane, &batch, 102., &api).unwrap();
    assert_eq!(failed["ci"]["state"], "failure");
    failed["phase"] = json!("ci_failed");
    failed["failure_token"] = json!(repair_token(&failed));
    check_failure(&t, &lane, &failed, &api).unwrap();
    api.0
        .get_mut("actions/runs/20/attempts/1/jobs?per_page=100&page=1")
        .unwrap()["jobs"][0]["conclusion"] = json!("cancelled");
    assert_eq!(
        observe(&t, &lane, &batch, 103., &api).unwrap()["state"],
        "needs_attention"
    );
}

fn merged_fixture() -> (J, J, J, J, Api) {
    let (t, lane, mut batch, mut api) = fixture();
    batch["ci"] = observe(&t, &lane, &batch, 100., &api).unwrap();
    assert_eq!(batch["ci"]["pr_membership"], "verified");
    let commit = json!("d".repeat(40));
    let landed = json!({"commit":commit});
    api.0.insert(
        format!(
            "compare/{}...{}",
            batch["base"].as_str().unwrap(),
            commit.as_str().unwrap()
        ),
        json!({"status":"ahead","merge_base_commit":{"sha":batch["base"]}}),
    );
    let pr = api.0.get_mut("pulls/7").unwrap();
    pr["state"] = json!("closed");
    pr["merged"] = json!(true);
    pr["base"]["sha"] = commit.clone();
    pr["merge_commit_sha"] = commit.clone();
    api.0.get_mut("branches/main").unwrap()["commit"]["sha"] = commit.clone();
    api.0.get_mut("actions/runs/10").unwrap()["pull_requests"] = json!([]);
    install_run(&mut api, &commit, "push", 20, 1, "success");
    (t, lane, batch, landed, api)
}

#[test]
fn premerge_membership_marker_is_only_written_on_strict_success() {
    let (t, lane, batch, api) = fixture();
    assert_eq!(
        observe(&t, &lane, &batch, 100., &api).unwrap()["pr_membership"],
        "verified"
    );
    for value in [
        json!([]),
        json!([{"number":8}]),
        json!(null),
        json!("7"),
        json!([null,{"number":7}]),
    ] {
        let mut bad = api.clone();
        bad.0.get_mut("actions/runs/10").unwrap()["pull_requests"] = value;
        assert!(observe(&t, &lane, &batch, 100., &bad).is_err());
    }
    let mut missing = api.clone();
    missing
        .0
        .get_mut("actions/runs/10")
        .unwrap()
        .as_object_mut()
        .unwrap()
        .remove("pull_requests");
    assert!(observe(&t, &lane, &batch, 100., &missing).is_err());
    let mut pending = api;
    pending.0.get_mut("actions/runs/10").unwrap()["status"] = json!("in_progress");
    assert!(observe(&t, &lane, &batch, 100., &pending)
        .unwrap()
        .get("pr_membership")
        .is_none());
}

#[test]
fn landing_empty_membership_requires_verified_proof_and_legacy_stays_strict() {
    let (t, lane, batch, landed, api) = merged_fixture();
    landing(&t, &lane, &batch, &landed, &api).unwrap();
    for marker in [json!(null), json!(false), json!("unverified")] {
        let mut legacy = batch.clone();
        legacy["ci"]["pr_membership"] = marker;
        assert!(landing(&t, &lane, &legacy, &landed, &api).is_err());
        let mut bound = api.clone();
        bound.0.get_mut("actions/runs/10").unwrap()["pull_requests"] = json!([{"number":7}]);
        landing(&t, &lane, &legacy, &landed, &bound).unwrap();
    }
    let mut legacy = batch.clone();
    legacy["ci"]
        .as_object_mut()
        .unwrap()
        .remove("pr_membership");
    assert!(landing(&t, &lane, &legacy, &landed, &api).is_err());
    for value in [
        json!([{"number":8}]),
        json!(null),
        json!({}),
        json!([null,{"number":7}]),
    ] {
        let mut bad = api.clone();
        bad.0.get_mut("actions/runs/10").unwrap()["pull_requests"] = value;
        assert!(landing(&t, &lane, &batch, &landed, &bad).is_err());
    }
    let mut bad = api.clone();
    bad.0
        .get_mut("actions/runs/10")
        .unwrap()
        .as_object_mut()
        .unwrap()
        .remove("pull_requests");
    assert!(landing(&t, &lane, &batch, &landed, &bad).is_err());
}

#[test]
fn landing_rejects_superseded_runs_and_changed_jobs_or_run_binding() {
    let (t, lane, batch, landed, api) = merged_fixture();
    for (id, attempt) in [(11, 1), (10, 2)] {
        let mut bad = api.clone();
        install_run(
            &mut bad,
            &batch["snapshot"]["head"],
            "pull_request",
            id,
            attempt,
            "success",
        );
        bad.0.get_mut(&format!("actions/runs/{id}")).unwrap()["pull_requests"] = json!([]);
        let err = landing(&t, &lane, &batch, &landed, &bad).unwrap_err();
        assert!(
            err.to_string()
                .contains("premerge run/attempt/workflow/jobs changed"),
            "{err}"
        );
    }
    for (key, value) in [
        ("id", json!(9001)),
        ("name", json!("other")),
        ("conclusion", json!("failure")),
        ("status", json!("in_progress")),
    ] {
        let mut bad = api.clone();
        bad.0
            .get_mut("actions/runs/10/attempts/1/jobs?per_page=100&page=1")
            .unwrap()["jobs"][0][key] = value;
        assert!(landing(&t, &lane, &batch, &landed, &bad).is_err());
    }
    for (path, value) in [
        ("/head_sha", json!("e".repeat(40))),
        ("/head_repository/full_name", json!("other/repo")),
        ("/head_branch", json!("other")),
        ("/path", json!(".github/workflows/other.yml")),
        ("/status", json!("in_progress")),
    ] {
        let mut bad = api.clone();
        *bad.0
            .get_mut("actions/runs/10")
            .unwrap()
            .pointer_mut(path)
            .unwrap() = value;
        assert!(landing(&t, &lane, &batch, &landed, &bad).is_err(), "{path}");
    }
}

#[test]
fn landing_rejects_incomplete_saved_proof_or_changed_owner_and_pr() {
    let (t, lane, batch, landed, api) = merged_fixture();
    for (path, value) in [
        ("/ci/state", json!("failure")),
        ("/ci/workflows", json!(null)),
        ("/ci/workflows", json!([])),
        ("/ci/workflows/0/state", json!("pending")),
        ("/ci/workflows/0/jobs", json!([])),
        ("/ci/workflows/0/attempt", json!(null)),
        ("/ci/workflows/0/file", json!("other.yml")),
        ("/ci/workflows/0/event", json!("push")),
        ("/ci/workflows/0/decision_terminal", json!(false)),
        ("/ci/policy_digest", json!("other")),
        ("/ci/binding/head", json!("e".repeat(40))),
        ("/ci/pr/task", json!("other")),
        ("/ci/pr/epoch", json!(3)),
        ("/ci/pr/number", json!(8)),
        ("/ci/pr/head_ref", json!("other")),
    ] {
        let mut bad = batch.clone();
        *bad.pointer_mut(path).unwrap() = value;
        assert!(landing(&t, &lane, &bad, &landed, &api).is_err(), "{path}");
    }
    for (path, value) in [
        ("/head/sha", json!("e".repeat(40))),
        ("/head/ref", json!("other")),
        ("/base/ref", json!("other")),
        ("/head/repo/full_name", json!("other/repo")),
        ("/state", json!("open")),
        ("/merged", json!(false)),
    ] {
        let mut bad = api.clone();
        *bad.0.get_mut("pulls/7").unwrap().pointer_mut(path).unwrap() = value;
        assert!(landing(&t, &lane, &batch, &landed, &bad).is_err(), "{path}");
    }
    for path in ["/ci/workflows/0/jobs", "/ci/workflows"] {
        let mut bad = batch.clone();
        let (parent, key) = path.rsplit_once('/').unwrap();
        bad.pointer_mut(parent)
            .unwrap()
            .as_object_mut()
            .unwrap()
            .remove(key);
        assert!(landing(&t, &lane, &bad, &landed, &api).is_err());
    }
}

#[test]
fn landing_revalidates_both_collections_and_merged_pr() {
    let (t, lane, batch, landed, api) = merged_fixture();
    for (path, after) in [("pulls/7", 1), ("actions/runs/10", 2)] {
        let changing = Changing {
            api: api.clone(),
            path: path.into(),
            after,
            count: Cell::new(0),
        };
        assert!(landing(&t, &lane, &batch, &landed, &changing).is_err());
    }
    let mut bad = api.clone();
    bad.0.insert(
        format!(
            "actions/workflows/rust.yml/runs?head_sha={}&event=pull_request&per_page=100&page=1",
            batch["snapshot"]["head"].as_str().unwrap()
        ),
        json!({"workflow_runs":[]}),
    );
    assert!(landing(&t, &lane, &batch, &landed, &bad).is_err());
}

struct StatusChanging(Api, Cell<usize>);
impl GitHubApi for StatusChanging {
    fn get(&self, path: &str) -> Result<J> {
        let mut row = self.0.get(path)?;
        if path == "actions/runs/10" {
            let n = self.1.get() + 1;
            self.1.set(n);
            if n % 2 == 0 {
                row["status"] = json!("in_progress");
            }
        }
        Ok(row)
    }
}
#[test]
fn landing_refuses_run_status_change_between_current_reads() {
    let (t, lane, batch, landed, api) = merged_fixture();
    let changing = StatusChanging(api, Cell::new(0));
    let err = landing(&t, &lane, &batch, &landed, &changing).unwrap_err();
    assert!(
        err.to_string()
            .contains("run status changed during observation"),
        "{err}"
    );
}
