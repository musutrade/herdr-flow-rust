//! Target slot admission is separate from both intake capacity and writers.
use super::*;

fn two_prepared(repo: &Repo, separate: &Repo) -> J {
    let mut s = prepared(repo);
    let mut foreign = prepared(separate)["tasks"]["o/r#1"].clone();
    foreign["task"] = json!("other");
    foreign["pr"]["head_ref"] = json!("other-feature");
    s["tasks"]["other"] = foreign;
    s
}

fn begin_other(s: &mut J) -> J {
    let spec = json!({"task":"other", "state_version":s["tasks"]["other"]["state_version"],
        "authorization":"publish"});
    pr::publication_begin(s, &spec, 21.0).unwrap()
}

fn assert_occupied(s: &mut J) {
    let before = s.clone();
    let error = begin(s).unwrap_err();
    assert!(
        error.to_string().contains("publication target is occupied"),
        "{error}"
    );
    assert_eq!(*s, before);
}

fn merged_foreign(repo: &Repo, separate: &Repo) -> J {
    let mut s = two_prepared(repo, separate);
    begin_other(&mut s);
    let mut pull = pr_json(separate, true);
    pull["head"]["ref"] = json!("other-feature");
    pull["state"] = json!("closed");
    let api = Fake::new()
        .with(
            "pulls?state=all&head=o%3Aother-feature&base=main&per_page=100&page=1",
            json!([pull.clone()]),
        )
        .with("pulls/7", pull)
        .with("branches/main", json!({"commit":{"sha":separate.head}}))
        .with(
            &format!("compare/{}...{}", separate.head, separate.head),
            json!({"status":"identical","merge_base_commit":{"sha":separate.head}}),
        );
    let spec = json!({"task":"other","state_version":s["tasks"]["other"]["state_version"]});
    pr::reconcile(&mut s, &spec, 22.0, Some(&api)).unwrap();
    assert_eq!(s["tasks"]["other"]["pr"]["phase"], "merged");
    s
}

#[test]
fn first_begin_claims_target_while_preparation_and_development_remain_parallel() {
    let repo = Repo::new();
    let separate = Repo::new();
    let mut s = two_prepared(&repo, &separate);
    begin_other(&mut s);
    assert_occupied(&mut s);
    assert!(reserve(&s, &repo.root, "o/r#1").is_ok());
    let third = Repo::new();
    assert!(reserve(&s, &third.root, "third").is_ok());
    // A queued prepared intent is allowed; it cannot start a second publication.
    s["tasks"]["o/r#1"] = task(&repo);
    pr::prepare(&mut s, &prepare_spec(&repo, 1), 23.0).unwrap();
    assert_occupied(&mut s);
    let before = s.clone();
    let spec = json!({"task":"other","state_version":s["tasks"]["other"]["state_version"],"authorization":"publish"});
    assert!(pr::publication_begin(&mut s, &spec, 24.0)
        .unwrap_err()
        .to_string()
        .contains("never replay"));
    assert_eq!(s, before);
}

#[test]
fn active_and_ambiguous_states_remain_occupied_regardless_of_task_control() {
    let repo = Repo::new();
    let separate = Repo::new();
    for (phase, op_phase, kind) in [
        ("publication_uncertain", "sending", "create"),
        ("publication_unconfirmed", "sending", "create"),
        ("registered", "observed", "create"),
        ("repair_needed", "observed", "create"),
        ("native_merge_not_enabled", "observed", "create"),
        ("conflict", "observed", "create"),
        ("base_changed", "observed", "create"),
        ("head_changed", "observed", "create"),
        ("closed_unmerged", "observed", "create"),
        ("publication_prepared", "prepared", "update"),
        ("update_unconfirmed", "sending", "update"),
        ("unknown", "unknown", "create"),
    ] {
        for control in ["running", "paused", "cancelled"] {
            let mut s = two_prepared(&repo, &separate);
            let foreign = &mut s["tasks"]["other"];
            foreign["control"] = json!(control);
            foreign["phase"] = json!("completed"); // Completion is not landing.
            foreign["writer"] = Value::Null;
            foreign["pr"]["phase"] = json!(phase);
            foreign["pr"]["publication"]["phase"] = json!(op_phase);
            foreign["pr"]["publication"]["kind"] = json!(kind);
            assert_occupied(&mut s);
        }
    }
}

#[test]
fn only_unstarted_create_preparation_is_free() {
    let repo = Repo::new();
    let separate = Repo::new();
    let mut s = two_prepared(&repo, &separate);
    s["tasks"]["other"]["control"] = json!("cancelled");
    begin(&mut s).unwrap(); // Cancellation is irrelevant: this never started.
    let mutations = [
        ("/number", json!(7)),
        ("/landing", json!({})),
        ("/remote", json!({})),
        ("/observation", json!({"state":"success"})),
        ("/publication/started_at", json!(1.0)),
        ("/publication/observed_at", json!(2.0)),
        ("/publication/previous_head", json!(repo.base)),
        ("/publication/operation", json!("repair")),
        ("/publication/kind", json!("update")),
        ("/publication/head", json!(repo.base)),
        ("/versions", json!([{}])),
        ("/repairs", json!({"used":{}})),
        ("/head", json!("bad-sha")),
        ("/publication/kind", json!("unknown")),
    ];
    for (pointer, value) in mutations {
        let mut s = two_prepared(&repo, &separate);
        set(&mut s["tasks"]["other"]["pr"], pointer, value);
        assert_occupied(&mut s);
    }
}

fn set(record: &mut J, pointer: &str, value: J) {
    let keys: Vec<_> = pointer.trim_start_matches('/').split('/').collect();
    let mut cursor = record;
    for key in &keys[..keys.len() - 1] {
        cursor = &mut cursor[*key];
    }
    cursor[keys[keys.len() - 1]] = value;
}

#[test]
fn observed_open_is_not_landing_and_closed_unmerged_is_not_release() {
    let repo = Repo::new();
    let separate = Repo::new();
    let mut s = two_prepared(&repo, &separate);
    begin_other(&mut s);
    let mut pull = pr_json(&separate, false);
    pull["head"]["ref"] = json!("other-feature");
    let api = Fake::new()
        .with(
            "pulls?state=all&head=o%3Aother-feature&base=main&per_page=100&page=1",
            json!([pull.clone()]),
        )
        .with("pulls/7", pull.clone());
    pr::reconcile(
        &mut s,
        &json!({"task":"other","state_version":3}),
        22.0,
        Some(&api),
    )
    .unwrap();
    assert_eq!(
        s["tasks"]["other"]["pr"]["publication"]["phase"],
        "observed"
    );
    assert_occupied(&mut s);
    pull["state"] = json!("closed");
    let api = Fake::new().with("pulls/7", pull);
    let spec = json!({"task":"other","state_version":s["tasks"]["other"]["state_version"]});
    pr::reconcile(&mut s, &spec, 23.0, Some(&api)).unwrap();
    assert_eq!(s["tasks"]["other"]["pr"]["phase"], "closed_unmerged");
    assert_occupied(&mut s);
}

#[test]
fn only_matching_reconciled_landing_releases_target() {
    let repo = Repo::new();
    let separate = Repo::new();
    let landed = merged_foreign(&repo, &separate);
    let mut s = landed.clone();
    begin(&mut s).unwrap();
    let mutations = [
        ("/landing/repo", json!("someone/else")),
        ("/landing/base_ref", json!("other")),
        ("/landing/number", json!(8)),
        ("/landing/head", json!("f".repeat(40))),
        ("/landing/commit", json!(null)),
        ("/landing/latest_base", json!("invalid")),
        ("/landing/observed_at", json!(0)),
        ("/landing/observed_at", json!(null)),
        ("/publication/observed_at", json!(23.0)),
        ("/remote/at", json!(23.0)),
        ("/remote/merged", json!(false)),
        ("/remote/state", json!("open")),
        ("/remote/head", json!("f".repeat(40))),
        ("/publication/head", json!("f".repeat(40))),
        ("/phase", json!("closed_unmerged")),
        ("/publication/phase", json!("sending")),
        ("/publication/kind", json!("unknown")),
        ("/number", json!(null)),
    ];
    for (pointer, value) in mutations {
        let mut s = landed.clone();
        set(&mut s["tasks"]["other"]["pr"], pointer, value);
        assert_occupied(&mut s);
    }
    // Fields must not become valid through null == null comparisons.
    let mut s = landed;
    s["tasks"]["other"]["pr"]["number"] = Value::Null;
    s["tasks"]["other"]["pr"]["landing"]["number"] = Value::Null;
    assert_occupied(&mut s);
}

#[test]
fn disjoint_targets_are_parallel_and_host_repo_case_does_not_split_a_target() {
    let repo = Repo::new();
    let separate = Repo::new();
    for (key, value) in [
        ("host", "enterprise.invalid"),
        ("repo", "different/repo"),
        ("base_ref", "release"),
    ] {
        let mut s = two_prepared(&repo, &separate);
        begin_other(&mut s);
        s["tasks"]["other"]["pr"][key] = json!(value);
        begin(&mut s).unwrap();
    }
    let mut s = two_prepared(&repo, &separate);
    begin_other(&mut s);
    s["tasks"]["other"]["pr"]["host"] = json!("GitHub.COM");
    s["tasks"]["other"]["pr"]["repo"] = json!("O/R");
    assert_occupied(&mut s);
}

#[test]
fn same_target_cli_contenders_admit_one_begin_under_the_queue_lock() {
    let repo = Repo::new();
    let separate = Repo::new();
    let s = two_prepared(&repo, &separate);
    let root = persistent_queue(&repo, s["tasks"].clone());
    let a = repo._tmp.path().join("begin-a.json");
    let b = repo._tmp.path().join("begin-b.json");
    for (path, name) in [(&a, "o/r#1"), (&b, "other")] {
        util::atomic_write_json(
            path,
            &json!({"task":name,"state_version":2,"authorization":"publish"}),
        )
        .unwrap();
    }
    let first = cli(&root, "pr-begin", &a).spawn().unwrap();
    let second = cli(&root, "pr-begin", &b).spawn().unwrap();
    let results = [
        first.wait_with_output().unwrap(),
        second.wait_with_output().unwrap(),
    ];
    assert_eq!(results.iter().filter(|r| r.status.success()).count(), 1);
    let loser = if results[0].status.success() { &b } else { &a };
    let before = std::fs::read(root.join("queue.json")).unwrap();
    let output = cli(&root, "pr-begin", loser).output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("publication target is occupied"));
    let spec = util::read_json(loser).unwrap();
    let _lock = queue::locked(&root, false).unwrap();
    let error = queue::task_action(&root, "pr-begin", &spec, Some("fixture-session"), &|_| {
        panic!("no model")
    })
    .unwrap_err();
    assert!(error.to_string().contains("publication target is occupied"));
    assert_eq!(std::fs::read(root.join("queue.json")).unwrap(), before);
    let s = queue::load_queue(&root).unwrap();
    assert_eq!(
        s["tasks"]
            .as_object()
            .unwrap()
            .values()
            .filter(|t| t["pr"]["publication"]["phase"] == "sending")
            .count(),
        1
    );
}

#[test]
fn same_task_update_can_begin_without_self_blocking_or_replaying() {
    use std::os::unix::fs::PermissionsExt;
    let repo = Repo::new();
    let separate = Repo::new();
    let mut s = two_prepared(&repo, &separate);
    let record = &mut s["tasks"]["o/r#1"]["pr"];
    record["number"] = json!(7);
    record["publication"] = json!({"kind":"update","phase":"prepared",
        "head":repo.head,"previous_head":repo.base,"operation":"repair"});
    let root = persistent_queue(&repo, s["tasks"].clone());
    let spec = repo._tmp.path().join("update.json");
    util::atomic_write_json(
        &spec,
        &json!({"task":"o/r#1","state_version":2,"authorization":"publish"}),
    )
    .unwrap();
    let bin = repo._tmp.path().join("bin");
    common::create_dir(&bin).unwrap();
    let gh = bin.join("gh");
    std::fs::write(&gh, "#!/bin/sh\n[ \"$*\" = 'api --hostname github.com --method GET repos/o/r/pulls/7' ] || exit 91\nprintf 'GET\\n' >> \"$SLOT_GET_LOG\"\ncat \"$SLOT_RESPONSE\"\n").unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o700)).unwrap();
    let response = repo._tmp.path().join("response.json");
    let log = repo._tmp.path().join("get.log");
    let mut pull = pr_json(&repo, false);
    pull["head"]["sha"] = json!(repo.base);
    util::atomic_write_json(&response, &pull).unwrap();
    let path = std::env::join_paths(
        std::iter::once(bin).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    let output = cli(&root, "pr-begin", &spec)
        .env("PATH", &path)
        .env("SLOT_RESPONSE", &response)
        .env("SLOT_GET_LOG", &log)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let s = queue::load_queue(&root).unwrap();
    let pr = &s["tasks"]["o/r#1"]["pr"];
    assert_eq!(pr["publication"]["phase"], "sending");
    assert_eq!(pr["publication"]["kind"], "update");
    assert_eq!(pr["number"], 7);
    assert_eq!(pr["head"], repo.head);
    util::atomic_write_json(
        &spec,
        &json!({"task":"o/r#1","state_version":3,"authorization":"publish"}),
    )
    .unwrap();
    let before = std::fs::read(root.join("queue.json")).unwrap();
    let output = cli(&root, "pr-begin", &spec)
        .env("PATH", &path)
        .env("SLOT_RESPONSE", &response)
        .env("SLOT_GET_LOG", &log)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("never replay"));
    assert_eq!(std::fs::read(root.join("queue.json")).unwrap(), before);
    assert_eq!(std::fs::read_to_string(log).unwrap(), "GET\n");
}
