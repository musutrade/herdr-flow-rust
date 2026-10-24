use super::*;

fn policy() -> J {
    json!({"kind":"github-actions","version":1,
        "workflows":[{"file":"rust.yml","event":"pull_request","decision":"required_jobs","jobs":["test","msrv"]}],
        "landing_workflows":[{"file":"rust.yml","event":"push","decision":"required_jobs","jobs":["test","msrv"]}]})
}
fn spec(s: &J) -> J {
    json!({"task":"lane","state_version":s["tasks"]["lane"]["state_version"],"batch":s["tasks"]["lane"]["lane"]["active"]["id"]})
}
fn snapshot(repo: &Repo, tmp: &TempDir) -> J {
    snapshot_with_policy(repo, tmp, policy())
}
fn snapshot_with_policy(repo: &Repo, tmp: &TempDir, ci: J) -> J {
    let mut owner = task(repo);
    owner["task"] = json!("lane");
    owner["candidate"] = json!(repo.base);
    owner["acceptance"] = json!({});
    let mut s = queue_state(task(repo));
    s["tasks"]["lane"] = owner;
    s["events"]["e1"] = json!({"id":"e1","wait":"step1","task":"o/r#1","epoch":1,"outcome":"complete","applied":true,"acknowledged":true,"binding":{"task":"o/r#1","epoch":1,"assignment":"a1","candidate":repo.head}});
    batch::lane::init(&mut s,&json!({"task":"lane","state_version":1,"repo":"o/r","host":"github.com","base_ref":"main","workspace_root":tmp.path().join("lane"),"capacity":1,"ci":ci}),10.).unwrap();
    assert!(s["tasks"]["lane"]["lane"]["gate"].is_null());
    let offer = json!({"task":"lane","state_version":s["tasks"]["lane"]["state_version"],"member":{"task":"o/r#1","state_version":1,"event":"e1","checkout":repo.root,"original_base":repo.base,"head":repo.head,"own_commits":[repo.head],"acceptance":["first"],"dependencies":[],"evidence":"/evidence/source"}});
    batch::action(&mut s, "batch-offer", &offer, 11., &mut |_| Ok(())).unwrap();
    let api = Fake::new().with("branches/main", json!({"commit":{"sha":repo.base}}));
    let input = spec(&s);
    batch::lane::freeze(&mut s, &input, 12., Some(&api)).unwrap();
    let b = s["tasks"]["lane"]["lane"]["active"].clone();
    let workspace = PathBuf::from(b["workspace"].as_str().unwrap());
    common::create_dir_all(&workspace).unwrap();
    util::atomic_write_json(
        &workspace.join("owner.json"),
        &json!({"batch":b["id"],"repo":"o/r","base":repo.base}),
    )
    .unwrap();
    let bare = workspace.join("objects.git");
    git(
        &workspace,
        &["init", "--bare", "-q", bare.to_str().unwrap()],
    );
    git(
        &bare,
        &["remote", "add", "origin", "https://github.com/o/r.git"],
    );
    for sha in [&repo.base, &repo.head] {
        git(
            &bare,
            &["fetch", "--no-tags", repo.root.to_str().unwrap(), sha],
        );
    }
    let input = spec(&s);
    batch::lane::integrate(&mut s, &input, 13., &mut |_| Ok(()), Some(&api)).unwrap();
    let mut input = spec(&s);
    input["issue"] = json!(112);
    batch::lane::snapshot(&mut s, &input, 14., Some(&api)).unwrap();
    s
}
fn run(mut api: Fake, sha: &str, event: &str, id: i64, attempt: i64, conclusion: &str) -> Fake {
    let value = json!({"id":id,"head_sha":sha,"run_attempt":attempt,"status":"completed","conclusion":conclusion,"event":event,"path":".github/workflows/rust.yml","head_branch":if event=="push" {json!("main")} else {api.get["pulls/7"]["head"]["ref"].clone()},"head_repository":{"full_name":"o/r"},"pull_requests":[{"number":7}]});
    let jobs:Vec<J>=["test","msrv"].iter().enumerate().map(|(i,name)|json!({"id":id*1000+attempt*100+i as i64,"name":name,"run_id":id,"head_sha":sha,"status":"completed","conclusion":conclusion})).collect();
    api = api
        .with(
            &format!(
                "actions/workflows/rust.yml/runs?head_sha={sha}&event={event}&per_page=100&page=1"
            ),
            json!({"workflow_runs":[value.clone()]}),
        )
        .with(
            &format!("actions/runs/{id}/attempts/{attempt}/jobs?per_page=100&page=1"),
            json!({"jobs":jobs}),
        )
        .with(&format!("actions/runs/{id}"), value);
    api
}
fn open_api(s: &J, repo: &Repo) -> Fake {
    let p = &s["tasks"]["lane"]["pr"];
    let remote = json!({"number":7,"state":"open","merged":false,"html_url":"https://github.com/o/r/pull/7",
        "head":{"sha":p["head"],"ref":p["head_ref"],"repo":{"full_name":"o/r"}},
        "base":{"sha":repo.base,"ref":"main","repo":{"full_name":"o/r"}},"auto_merge":null});
    Fake::new()
        .with("branches/main", json!({"commit":{"sha":repo.base}}))
        .with("pulls/7", remote.clone())
        .with(
            &format!(
                "pulls?state=all&head=o%3A{}&base=main&per_page=100&page=1",
                herdr_flow::util::pyjson::quote_component(p["head_ref"].as_str().unwrap())
            ),
            json!([remote]),
        )
}
fn register(s: &mut J, api: &Fake) {
    let mut input = spec(s);
    input["number"] = json!(7);
    pr::reconcile(s, &input, 16., Some(api)).unwrap();
}

#[test]
fn omitted_authorization_recovery_allows_exact_frozen_lane_begin_once() {
    use herdr_flow::queue;
    use std::os::unix::fs::PermissionsExt;
    let repo = Repo::new();
    let tmp = common::tempdir().unwrap();
    let mut s = snapshot(&repo, &tmp);
    s["tasks"]["lane"]["schema_version"] = json!(1);
    s["tasks"]["lane"]["authorization"] =
        json!({"review":{"action":"verification","source":"user"}});
    let lane_before = s["tasks"]["lane"]["lane"].clone();
    let source_before = s["tasks"]["o/r#1"].clone();
    let guard = tmp.path().join("guard");
    common::create_dir(&guard).unwrap();
    util::atomic_write_json(
        &guard.join("state.json"),
        &json!({"version":2,
        "phase":"monitoring","session":"fixture-session","task":"lane",
        "role":"coordinator","pane":"w1:p1","cwd":repo.root}),
    )
    .unwrap();
    let root = tmp.path().join("queue");
    let mut q = queue::init(&root, Some(&guard), 30).unwrap();
    q["tasks"] = s["tasks"].clone();
    q["events"] = s["events"].clone();
    queue::save(&root, &mut q).unwrap();
    let bin = tmp.path().join("bin");
    common::create_dir(&bin).unwrap();
    let gh = bin.join("gh");
    std::fs::write(&gh, "#!/bin/sh\n[ \"$*\" = 'api --hostname github.com --method GET repos/o/r/branches/main' ] || exit 91\nprintf 'GET\\n' >> \"$AUTH_GET_LOG\"\ncat \"$AUTH_RESPONSE\"\n").unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o700)).unwrap();
    let response = tmp.path().join("response.json");
    util::atomic_write_json(&response, &json!({"commit":{"sha":repo.base}})).unwrap();
    let log = tmp.path().join("get.log");
    let path = std::env::join_paths(
        std::iter::once(bin).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    let input_path = tmp.path().join("spec.json");
    let run = |action: &str, input: &J| {
        util::atomic_write_json(&input_path, input).unwrap();
        Command::new(env!("CARGO_BIN_EXE_herdr-flow"))
            .args(["queue", action, "--state-dir"])
            .arg(&root)
            .arg("--spec")
            .arg(&input_path)
            .env_remove("OPENCODE_SESSION_ID")
            .env("CODEX_THREAD_ID", "fixture-session")
            .env("PATH", &path)
            .env("AUTH_RESPONSE", &response)
            .env("AUTH_GET_LOG", &log)
            .output()
            .unwrap()
    };
    let mut input = spec(&s);
    input["authorization"] = json!("publish");
    let before = std::fs::read(root.join("queue.json")).unwrap();
    let failed = run("pr-begin", &input);
    assert!(!failed.status.success());
    assert!(String::from_utf8_lossy(&failed.stderr).contains("explicit publication authorization"));
    assert_eq!(std::fs::read(root.join("queue.json")).unwrap(), before);
    assert!(!log.exists());
    let authorization = json!({"task":"lane","state_version":input["state_version"],
        "id":"publish","action":"publication","source":"Existing user approval: fixture reference"});
    let added = run("task-authorize", &authorization);
    assert!(
        added.status.success(),
        "{}",
        String::from_utf8_lossy(&added.stderr)
    );
    let q = queue::load_queue(&root).unwrap();
    assert_eq!(q["tasks"]["lane"]["lane"], lane_before);
    assert_eq!(q["tasks"]["o/r#1"], source_before);
    input["state_version"] = q["tasks"]["lane"]["state_version"].clone();
    let started = run("pr-begin", &input);
    assert!(
        started.status.success(),
        "{}",
        String::from_utf8_lossy(&started.stderr)
    );
    let q = queue::load_queue(&root).unwrap();
    assert_eq!(q["tasks"]["lane"]["pr"]["publication"]["phase"], "sending");
    assert_eq!(q["tasks"]["lane"]["lane"], lane_before);
    assert_eq!(q["tasks"]["o/r#1"], source_before);
    input["state_version"] = q["tasks"]["lane"]["state_version"].clone();
    let before = std::fs::read(root.join("queue.json")).unwrap();
    let failed = run("pr-begin", &input);
    assert!(!failed.status.success());
    assert!(String::from_utf8_lossy(&failed.stderr).contains("never replay"));
    assert_eq!(std::fs::read(root.join("queue.json")).unwrap(), before);
    assert_eq!(std::fs::read_to_string(log).unwrap(), "GET\n");
}

#[test]
fn actions_lane_publication_and_landing_retain_slot_until_main_ci() {
    let repo = Repo::new();
    let tmp = common::tempdir().unwrap();
    let mut s = snapshot(&repo, &tmp);
    let input = spec(&s);
    let api = open_api(&s, &repo);
    let unchanged = s.clone();
    assert!(batch::lane::ci_observe(&mut s, &input, 15., Some(&api)).is_err());
    assert_eq!(s, unchanged);
    batch::validation::publication_check(&s, &s["tasks"]["lane"], 15., Some(&api)).unwrap();
    register(&mut s, &api);
    let head = s["tasks"]["lane"]["pr"]["head"]
        .as_str()
        .unwrap()
        .to_owned();
    let api = run(api, &head, "pull_request", 10, 1, "success");
    let input = spec(&s);
    let mut empty = Fake {
        get: api.get.clone(),
    };
    empty.get.get_mut("actions/runs/10").unwrap()["pull_requests"] = json!([]);
    let unchanged = s.clone();
    assert!(batch::lane::ci_observe(&mut s, &input, 17., Some(&empty)).is_err());
    assert_eq!(
        s, unchanged,
        "OPEN empty refuses without any new observation/marker"
    );
    batch::lane::ci_observe(&mut s, &input, 17., Some(&api)).unwrap();
    assert_eq!(
        s["tasks"]["lane"]["lane"]["active"]["ci"]["pr_membership"],
        "verified"
    );
    assert_eq!(s["tasks"]["lane"]["lane"]["active"]["phase"], "ci_passed");
    assert!(s["tasks"]["lane"]["lane"]["active"]["gate"].is_null());
    let first_batch = s["tasks"]["lane"]["lane"]["active"]["id"].clone();
    let next_checkout = tmp.path().join("independent");
    git(
        &repo.root,
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            next_checkout.to_str().unwrap(),
            &repo.base,
        ],
    );
    std::fs::write(
        next_checkout.join("next"),
        "independent work while CI owns active slot",
    )
    .unwrap();
    git(&next_checkout, &["add", "next"]);
    git(&next_checkout, &["commit", "-qm", "next independent issue"]);
    let next_head = output(&next_checkout, &["rev-parse", "HEAD"]);
    let mut next = task(&repo);
    next["task"] = json!("next");
    next["candidate"] = json!(next_head);
    next["wait"] = json!("next-result");
    s["tasks"]["next"] = next;
    s["events"]["next-e"] = json!({"id":"next-e","task":"next","wait":"next-result","epoch":1,"outcome":"complete","applied":true,"acknowledged":true,"binding":{"task":"next","epoch":1,"assignment":"a1","candidate":next_head}});
    let offer = json!({"task":"lane","state_version":s["tasks"]["lane"]["state_version"],"member":{"task":"next","state_version":1,"event":"next-e","checkout":next_checkout,"original_base":repo.base,"head":next_head,"own_commits":[next_head],"dependencies":[],"acceptance":["first"],"evidence":"/evidence/next"}});
    batch::action(&mut s, "batch-offer", &offer, 17.5, &mut |_| Ok(())).unwrap();
    assert_eq!(s["tasks"]["lane"]["lane"]["active"]["id"], first_batch);
    assert_eq!(
        s["tasks"]["lane"]["lane"]["pending"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let mut full = offer.clone();
    full["state_version"] = s["tasks"]["lane"]["state_version"].clone();
    let unchanged = s.clone();
    assert!(batch::action(&mut s, "batch-offer", &full, 17.6, &mut |_| Ok(())).is_err());
    assert_eq!(s, unchanged);
    let receipt = s["tasks"]["o/r#1"]["batch_delivery"]["receipt_id"].clone();
    let landing = "d".repeat(40);
    let tree = s["tasks"]["lane"]["lane"]["active"]["snapshot"]["tree"].clone();
    let mut remote = api.get["pulls/7"].clone();
    remote["state"] = json!("closed");
    remote["merged"] = json!(true);
    remote["merge_commit_sha"] = json!(landing);
    remote["base"]["sha"] = json!(landing);
    let api = api
        .with("pulls/7", remote)
        .with("branches/main", json!({"commit":{"sha":landing}}))
        .with(
            &format!("compare/{landing}...{landing}"),
            json!({"status":"identical","merge_base_commit":{"sha":landing}}),
        )
        .with(
            &format!("compare/{}...{landing}", repo.base),
            json!({"status":"ahead","merge_base_commit":{"sha":repo.base}}),
        )
        .with(
            &format!("git/commits/{landing}"),
            json!({"tree":{"sha":tree}}),
        );
    let mut api = api;
    api.get.get_mut("actions/runs/10").unwrap()["pull_requests"] = json!([]);
    let api = run(api, &landing, "push", 20, 1, "failure");
    let input = spec(&s);
    let unchanged = s.clone();
    assert!(batch::lane::reconcile(&mut s, &input, 18., Some(&api)).is_err());
    assert_eq!(
        s, unchanged,
        "failed main CI must retain exact queue and active slot"
    );
    let api = run(api, &landing, "push", 20, 2, "success");
    let mut legacy = s.clone();
    legacy["tasks"]["lane"]["lane"]["active"]["ci"]
        .as_object_mut()
        .unwrap()
        .remove("pr_membership");
    let unchanged = legacy.clone();
    assert!(batch::lane::reconcile(&mut legacy, &input, 18.5, Some(&api)).is_err());
    assert_eq!(
        legacy, unchanged,
        "legacy empty membership must retain exact queue/slot"
    );
    for (path, value) in [
        (
            format!("git/commits/{landing}"),
            json!({"tree":{"sha":"f".repeat(40)}}),
        ),
        (
            format!("compare/{}...{landing}", repo.base),
            json!({"status":"diverged","merge_base_commit":{"sha":repo.base}}),
        ),
        (
            format!(
                "actions/workflows/rust.yml/runs?head_sha={landing}&event=push&per_page=100&page=1"
            ),
            json!({"workflow_runs":[]}),
        ),
    ] {
        let bad = Fake {
            get: api.get.clone(),
        }
        .with(&path, value);
        let unchanged = s.clone();
        assert!(batch::lane::reconcile(&mut s, &input, 18.5, Some(&bad)).is_err());
        assert_eq!(s, unchanged);
    }
    let mut missing = s.clone();
    missing["tasks"]["lane"]["lane"]["active"]["ci"] = json!(null);
    let unchanged = missing.clone();
    assert!(batch::lane::reconcile(&mut missing, &input, 18.5, Some(&api)).is_err());
    assert_eq!(missing, unchanged);
    let landed = batch::lane::reconcile(&mut s, &input, 19., Some(&api)).unwrap();
    assert_ne!(landed["lane"]["active"]["id"], first_batch);
    assert_eq!(landed["lane"]["active"]["phase"], "integration_prepared");
    assert_eq!(landed["lane"]["active"]["members"][0]["task"], "next");
    assert_eq!(landed["lane"]["pending"], json!([]));
    assert_eq!(s["tasks"]["o/r#1"]["batch_delivery"]["receipt_id"], receipt);
    assert_eq!(s["tasks"]["o/r#1"]["batch_delivery"]["phase"], "landed");
}
#[test]
fn actions_observation_failure_is_atomic_and_gate_actions_are_rejected() {
    let repo = Repo::new();
    let tmp = common::tempdir().unwrap();
    let mut s = snapshot(&repo, &tmp);
    let api = open_api(&s, &repo);
    register(&mut s, &api);
    let unchanged = s.clone();
    let input = spec(&s);
    assert!(batch::lane::ci_observe(&mut s, &input, 20., Some(&api)).is_err());
    assert_eq!(s, unchanged);
    assert!(batch::action(&mut s, "batch-gate-begin", &input, 20., &mut |_| Ok(())).is_err());
    assert!(batch::action(&mut s, "batch-gate-record", &input, 20., &mut |_| Ok(())).is_err());
    assert_eq!(s, unchanged);
    let head = s["tasks"]["lane"]["pr"]["head"]
        .as_str()
        .unwrap()
        .to_owned();
    let api = run(api, &head, "pull_request", 10, 1, "failure");
    batch::lane::ci_observe(&mut s, &input, 21., Some(&api)).unwrap();
    assert_eq!(s["tasks"]["lane"]["lane"]["active"]["phase"], "ci_failed");
    assert_eq!(
        s["tasks"]["lane"]["lane"]["active"]["progress_deadline"],
        json!(621.)
    );
    assert_eq!(batch::lane::stall(&s["tasks"]["lane"], 620.), None);
    assert_eq!(
        batch::lane::stall(&s["tasks"]["lane"], 622.),
        Some("batch_ci_observation_overdue".to_string())
    );
    let token = s["tasks"]["lane"]["lane"]["active"]["failure_token"].clone();
    let input = spec(&s);
    batch::lane::ci_observe(&mut s, &input, 22., Some(&api)).unwrap();
    assert_ne!(s["tasks"]["lane"]["lane"]["active"]["failure_token"], token);
    s["tasks"]["lane"]["lane"]["active"]["ci_observations"] = json!(256);
    let unchanged = s.clone();
    let input = spec(&s);
    assert!(batch::lane::ci_observe(&mut s, &input, 23., Some(&api)).is_err());
    assert_eq!(s, unchanged);
}

#[test]
fn actions_repair_revalidates_failure_at_import_and_ref_update() {
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
            "phase": "snapshot_frozen",
            "workspace": workspace,
            "members": [],
            "versions": [],
            "corrections": [],
            "integration": {"mapping": [], "offset": 0}
        }
    });
    let ci = policy();
    owner["lane"]["ci_policy_digest"] = json!(util::pyjson::sha256_json_sorted(&ci));
    owner["lane"]["ci"] = ci;
    owner["lane"]["active"]["snapshot"] = json!({"head":repo.base,"tree":output(&repo.root,&["rev-parse",&format!("{}^{{tree}}",repo.base)])});
    owner["pr"] = json!({"number":7,"phase":"registered","head":repo.base,"head_ref":"feature","repo":"o/r","host":"github.com","base_ref":"main","target_base":repo.base});
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
    let api = run(Fake::new().with("branches/main",json!({"commit":{"sha":repo.base}})).with("pulls/7",json!({"number":7,"state":"open","merged":false,"head":{"sha":repo.base,"ref":"feature","repo":{"full_name":"o/r"}},"base":{"sha":repo.base,"ref":"main","repo":{"full_name":"o/r"}}})),&repo.base,"pull_request",10,1,"failure");
    batch::lane::ci_observe(
        &mut s,
        &json!({"task":"o/r#1","state_version":1,"batch":"batch-repair"}),
        19.,
        Some(&api),
    )
    .unwrap();
    s["tasks"]["o/r#1"]["operations"]["repair-op"]["intent"]["batch_repair"]["token"] =
        s["tasks"]["o/r#1"]["lane"]["active"]["failure_token"].clone();
    let repaired = batch::lane::repair_with_api(
        &mut s,
        &json!({
            "task": "o/r#1", "state_version": 2, "batch": "batch-repair",
            "operation": "repair-op", "event": "repair-result", "checkout": repo.root,
            "head": repo.head, "evidence": "verified isolated repair"
        }),
        20.0,
        Some(&api),
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

    let changed = run(
        Fake {
            get: api.get.clone(),
        },
        &repo.base,
        "pull_request",
        10,
        2,
        "failure",
    )
    .get;
    let unchanged = s.clone();
    let finish_spec =
        json!({"task":"o/r#1","state_version":repaired["state_version"],"batch":"batch-repair"});
    let changed_error = batch::lane::repair_finish_with_api(
        &mut s,
        &finish_spec,
        21.,
        &mut |_| Ok(()),
        Some(&Fake { get: changed }),
    )
    .unwrap_err();
    assert!(
        changed_error
            .to_string()
            .contains("CI failure/run/attempt changed"),
        "{changed_error}"
    );
    assert_eq!(s, unchanged);
    assert_eq!(
        output(&bare, &["rev-parse", "refs/herdr/candidate"]),
        repo.base
    );
    let finished = batch::lane::repair_finish_with_api(
        &mut s,
        &json!({"task": "o/r#1", "state_version": repaired["state_version"], "batch": "batch-repair"}),
        21.0,
        &mut |_| Ok(()), Some(&api),
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
    assert!(snapshot["lane"]["active"]["ci"].is_null());
    assert_eq!(snapshot["pr"]["number"], 7);
    assert_eq!(snapshot["pr"]["publication"]["kind"], "update");
}

#[test]
fn actions_rebase_clears_evidence_and_retains_same_pr_for_new_snapshot() {
    let repo = Repo::new();
    let tmp = common::tempdir().unwrap();
    let mut s = snapshot(&repo, &tmp);
    let api = open_api(&s, &repo);
    register(&mut s, &api);
    let old_head = s["tasks"]["lane"]["pr"]["head"]
        .as_str()
        .unwrap()
        .to_owned();
    let api = run(api, &old_head, "pull_request", 10, 1, "success");
    let input = spec(&s);
    batch::lane::ci_observe(&mut s, &input, 20., Some(&api)).unwrap();
    let target = tmp.path().join("new-target");
    git(
        &repo.root,
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            target.to_str().unwrap(),
            &repo.base,
        ],
    );
    std::fs::write(target.join("target-change"), "new target").unwrap();
    git(&target, &["add", "target-change"]);
    git(&target, &["commit", "-qm", "target advances"]);
    let base = output(&target, &["rev-parse", "HEAD"]);
    let mut remote = api.get["pulls/7"].clone();
    remote["base"]["sha"] = json!(base);
    let api = api
        .with("branches/main", json!({"commit":{"sha":base}}))
        .with("pulls/7", remote)
        .with(
            &format!("compare/{}...{}", repo.base, base),
            json!({"status":"ahead","merge_base_commit":{"sha":repo.base}}),
        );
    s["tasks"]["lane"]["lane"]["active"]["ci_repair_source"] = json!({"obsolete":true});
    let input = spec(&s);
    let rebased = batch::lane::rebase(&mut s, &input, 21., Some(&api)).unwrap();
    let b = &rebased["lane"]["active"];
    assert!(b.get("ci").is_none());
    assert!(b.get("ci_repair_source").is_none());
    assert_eq!(b["previous_pr"]["number"], 7);
    assert_eq!(b["ci_observations"], 1);
    let workspace = PathBuf::from(b["workspace"].as_str().unwrap());
    common::create_dir_all(&workspace).unwrap();
    util::atomic_write_json(
        &workspace.join("owner.json"),
        &json!({"batch":b["id"],"repo":"o/r","base":base}),
    )
    .unwrap();
    let bare = workspace.join("objects.git");
    git(
        &workspace,
        &["init", "--bare", "-q", bare.to_str().unwrap()],
    );
    git(
        &bare,
        &["remote", "add", "origin", "https://github.com/o/r.git"],
    );
    for sha in [&repo.base, &repo.head, &base] {
        git(
            &bare,
            &["fetch", "--no-tags", repo.root.to_str().unwrap(), sha],
        );
    }
    let input = spec(&s);
    batch::lane::integrate(&mut s, &input, 22., &mut |_| Ok(()), Some(&api)).unwrap();
    let mut input = spec(&s);
    input["issue"] = json!(112);
    let fresh = batch::lane::snapshot(&mut s, &input, 23., Some(&api)).unwrap();
    assert_eq!(fresh["pr"]["number"], 7);
    assert_eq!(fresh["pr"]["publication"]["kind"], "update");
    assert_eq!(fresh["pr"]["target_base"], base);
    assert!(fresh["lane"]["active"]["ci"].is_null());
    batch::validation::publication_check(&s, &fresh, 24., Some(&api)).unwrap();
    let mut auto = api.get.clone();
    auto.get_mut("pulls/7").unwrap()["auto_merge"] = json!({"merge_method":"squash"});
    assert!(
        batch::validation::publication_check(&s, &fresh, 24., Some(&Fake { get: auto })).is_err()
    );
}

#[test]
fn actions_multiple_workflows_do_not_mint_early_failure_token() {
    let repo = Repo::new();
    let tmp = common::tempdir().unwrap();
    let mut ci = policy();
    let mut second = ci["workflows"][0].clone();
    second["file"] = json!("other.yml");
    ci["workflows"].as_array_mut().unwrap().push(second);
    let mut s = snapshot_with_policy(&repo, &tmp, ci);
    let api = open_api(&s, &repo);
    register(&mut s, &api);
    let head = s["tasks"]["lane"]["pr"]["head"]
        .as_str()
        .unwrap()
        .to_owned();
    let mut api = run(api, &head, "pull_request", 10, 1, "failure");
    let path = format!(
        "actions/workflows/other.yml/runs?head_sha={head}&event=pull_request&per_page=100&page=1"
    );
    api.get.insert(path, json!({"workflow_runs":[]}));
    let input = spec(&s);
    batch::lane::ci_observe(&mut s, &input, 20., Some(&api)).unwrap();
    let active = &s["tasks"]["lane"]["lane"]["active"];
    assert_eq!(active["phase"], "ci_pending");
    assert!(active["failure_token"].is_null());
    let other = run(open_api(&s, &repo), &head, "pull_request", 20, 1, "success");
    for (key, mut value) in other.get {
        if !key.starts_with("actions/") {
            continue;
        }
        if let Some(runs) = value.get_mut("workflow_runs") {
            runs[0]["path"] = json!(".github/workflows/other.yml");
        } else if value.get("path").is_some() {
            value["path"] = json!(".github/workflows/other.yml");
        }
        api.get.insert(
            key.replace("workflows/rust.yml/", "workflows/other.yml/"),
            value,
        );
    }
    for (status, phase) in [
        ("in_progress", "ci_pending"),
        ("completed", "ci_failed"),
        ("in_progress", "ci_pending"),
    ] {
        api.get.get_mut("actions/runs/20").unwrap()["status"] = json!(status);
        let input = spec(&s);
        batch::lane::ci_observe(&mut s, &input, 21., Some(&api)).unwrap();
        let active = &s["tasks"]["lane"]["lane"]["active"];
        assert_eq!(active["phase"], phase);
        if phase == "ci_failed" {
            assert!(!active["failure_token"].as_str().unwrap().is_empty());
        } else {
            assert!(
                active["failure_token"].is_null(),
                "no new or stale repair token"
            );
        }
    }
}
