//! No model, HTTP requests, or publication: real Git and local queue fixtures.
use super::*;
use herdr_flow::{progress, queue};

#[path = "target_slot.rs"]
mod target_slot;

fn begin(s: &mut J) -> herdr_flow::Result<J> {
    let spec = json!({"task":"o/r#1", "state_version":s["tasks"]["o/r#1"]["state_version"],
        "authorization":"publish"});
    pr::publication_begin(s, &spec, 20.0)
}

fn prepared(repo: &Repo) -> J {
    let mut s = queue_state(task(repo));
    pr::prepare(&mut s, &prepare_spec(repo, 1), 10.0).unwrap();
    s
}

fn other(repo: &Repo) -> J {
    let mut t = task(repo);
    t["task"] = json!("other");
    t
}

fn writer(path: &Path, session: &str) -> J {
    json!({"workspace":path, "assignment":"a1", "session":session})
}

fn reserve(s: &J, path: &Path, owner: &str) -> herdr_flow::Result<J> {
    progress::reserve_writer(s, path.to_str().unwrap(), "a1", "same-session", owner)
}

fn refuse_at_prepare_and_begin(repo: &Repo, foreign: J, message: &str) {
    let mut s = queue_state(task(repo));
    s["tasks"]["other"] = foreign.clone();
    let before = s.clone();
    let error = pr::prepare(&mut s, &prepare_spec(repo, 1), 10.0).unwrap_err();
    assert!(error.to_string().contains(message), "{error}");
    assert_eq!(s, before);

    // Also covers a writer or legacy duplicate appearing after preparation.
    let mut s = prepared(repo);
    s["tasks"]["other"] = foreign;
    let before = s.clone();
    let error = begin(&mut s).unwrap_err();
    assert!(error.to_string().contains(message), "{error}");
    assert_eq!(s, before);
}

#[test]
fn publication_rejects_foreign_writer_regardless_of_session_or_control() {
    let repo = Repo::new();
    for (control, phase) in [
        ("running", "blocked"),
        ("paused", "blocked"),
        ("cancelled", "blocked"),
        ("running", "completed"),
    ] {
        for session in ["same-session", "different-session"] {
            let mut foreign = other(&repo);
            foreign["control"] = json!(control);
            foreign["phase"] = json!(phase);
            foreign["writer"] = writer(&repo.root, session);
            refuse_at_prepare_and_begin(&repo, foreign, "another task's writer");
        }
    }
}

#[test]
fn publication_resolves_symlinks_and_both_directions_of_overlap() {
    let repo = Repo::new();
    let child = repo.root.join("sub");
    common::create_dir(&child).unwrap();
    let alias = repo._tmp.path().join("alias");
    std::os::unix::fs::symlink(&repo.root, &alias).unwrap();
    for path in [
        repo.root.clone(),
        repo._tmp.path().to_path_buf(),
        child,
        alias.clone(),
        alias.join("sub/.."),
        repo.root.join("."),
    ] {
        let mut foreign = other(&repo);
        foreign["writer"] = writer(&path, "same-session");
        refuse_at_prepare_and_begin(&repo, foreign, "another task's writer");
    }
    let mut s = queue_state(task(&repo));
    let mut spec = prepare_spec(&repo, 1);
    spec["checkout"] = json!(alias);
    s["tasks"]["other"] = other(&repo);
    s["tasks"]["other"]["writer"] = writer(&repo.root, "same-session");
    assert!(pr::prepare(&mut s, &spec, 10.0)
        .unwrap_err()
        .to_string()
        .contains("another task's writer"));
}

#[test]
fn malformed_foreign_ownership_fails_closed_without_mutating_state() {
    let repo = Repo::new();
    let alias = repo._tmp.path().join("loop");
    std::os::unix::fs::symlink(&alias, &alias).unwrap();
    for bad in [
        json!({}),
        json!(false),
        json!("writer"),
        json!({"workspace":"relative"}),
        json!({"workspace":repo.root.join("missing")}),
        json!({"workspace":repo.root.join("a")}),
        json!({"workspace":alias}),
    ] {
        let mut foreign = other(&repo);
        foreign["writer"] = bad;
        refuse_at_prepare_and_begin(&repo, foreign, "ownership");
    }
    let mut foreign = other(&repo);
    foreign["task"] = json!("o/r#1"); // Same-task exemption cannot be spoofed.
    refuse_at_prepare_and_begin(&repo, foreign, "ownership identity");
    for bad in [
        json!({}),
        json!(false),
        json!({"host":"github.com", "repo":"o/r"}),
    ] {
        let mut foreign = other(&repo);
        foreign["pr"] = bad;
        refuse_at_prepare_and_begin(&repo, foreign, "PR");
    }
}

#[test]
fn cancel_preserves_writer_and_release_is_explicit() {
    let repo = Repo::new();
    let mut s = prepared(&repo);
    s["tasks"]["other"] = other(&repo);
    s["tasks"]["other"]["writer"] = writer(&repo.root, "same-session");
    progress::control(
        &mut s,
        Some(&json!("other")),
        Some(&json!(1)),
        Some(&json!("cancelled")),
        Some(&json!("superseded")),
        21.0,
    )
    .unwrap();
    assert!(begin(&mut s).is_err());
    assert!(!s["tasks"]["other"]["writer"].is_null());
    progress::release(
        &mut s,
        &json!({"task":"other", "state_version":2,
        "evidence":"inspected stopped writer"}),
        22.0,
    )
    .unwrap();
    assert!(begin(&mut s).is_ok());
    let before = s.clone();
    assert!(begin(&mut s)
        .unwrap_err()
        .to_string()
        .contains("never replay"));
    assert_eq!(s, before);
}

#[test]
fn retained_intent_blocks_duplicates_even_after_cancel_release_and_landing() {
    let repo = Repo::new();
    let separate = Repo::new();
    let record = prepared(&repo)["tasks"]["o/r#1"]["pr"].clone();
    for phase in ["prepared", "sending", "observed"] {
        let mut foreign = other(&repo);
        foreign["control"] = json!("cancelled");
        foreign["writer"] = Value::Null;
        foreign["pr"] = record.clone();
        foreign["pr"]["checkout"] = json!(separate.root);
        foreign["pr"]["head"] = json!("f".repeat(40));
        foreign["pr"]["publication"]["phase"] = json!(phase);
        foreign["pr"]["phase"] = json!("merged");
        refuse_at_prepare_and_begin(&repo, foreign, "intent is owned by another task");
    }
    let mut foreign = other(&repo);
    foreign["pr"] = record;
    foreign["pr"]["host"] = json!("GitHub.COM");
    foreign["pr"]["repo"] = json!("O/R");
    refuse_at_prepare_and_begin(&repo, foreign, "intent is owned by another task");
}

#[test]
fn distinct_branches_keep_development_parallel_but_serialize_target_publication() {
    let repo = Repo::new();
    let separate = Repo::new();
    let mut s = prepared(&repo);
    s["tasks"]["other"] = other(&separate);
    let mut pr = s["tasks"]["o/r#1"]["pr"].clone();
    pr["head_ref"] = json!("independent");
    pr["checkout"] = json!(separate.root);
    pr["publication"]["phase"] = json!("sending");
    s["tasks"]["other"]["pr"] = pr;
    s["tasks"]["other"]["writer"] = writer(&separate.root, "same-session");
    s["tasks"]["o/r#1"] = task(&repo);
    pr::prepare(&mut s, &prepare_spec(&repo, 1), 11.0).unwrap();
    let before = s.clone();
    assert!(begin(&mut s)
        .unwrap_err()
        .to_string()
        .contains("publication target is occupied"));
    assert_eq!(s, before);
    let third = Repo::new();
    assert!(reserve(&s, &third.root, "third").is_ok());
    // Own-task repair reservation is not a foreign ownership conflict.
    assert!(reserve(&s, &repo.root, "o/r#1").is_ok());
}

#[test]
fn released_abandoned_preparation_allows_a_distinct_branch() {
    let repo = Repo::new();
    let mut s = prepared(&repo);
    let mut foreign = s["tasks"]["o/r#1"].clone();
    foreign["task"] = json!("other");
    foreign["control"] = json!("cancelled");
    foreign["writer"] = writer(&repo.root, "same-session");
    foreign["pr"]["head_ref"] = json!("abandoned-branch");
    s["tasks"]["other"] = foreign;
    progress::release(
        &mut s,
        &json!({"task":"other", "state_version":2,
        "evidence":"inspected no sending publication and writer stopped"}),
        21.0,
    )
    .unwrap();
    assert!(begin(&mut s).is_ok());
    assert_eq!(s["tasks"]["other"]["pr"]["head_ref"], "abandoned-branch");
}

#[test]
fn sending_publication_blocks_later_writer_and_cancel_does_not_release_it() {
    let repo = Repo::new();
    let mut s = prepared(&repo);
    assert!(reserve(&s, &repo.root, "other").is_ok()); // Prepared != sending.
    begin(&mut s).unwrap();
    let alias = repo._tmp.path().join("alias");
    std::os::unix::fs::symlink(&repo.root, &alias).unwrap();
    let sub = repo.root.join("sub");
    common::create_dir(&sub).unwrap();
    progress::control(
        &mut s,
        Some(&json!("o/r#1")),
        Some(&json!(3)),
        Some(&json!("cancelled")),
        Some(&json!("cancel is not reconciliation")),
        21.0,
    )
    .unwrap();
    progress::release(
        &mut s,
        &json!({"task":"o/r#1", "state_version":4,
        "evidence":"writer stopped, publication still uncertain"}),
        22.0,
    )
    .unwrap();
    let before = s.clone();
    for path in [
        repo.root.clone(),
        alias,
        sub,
        repo._tmp.path().to_path_buf(),
    ] {
        let error = reserve(&s, &path, "other").unwrap_err();
        assert!(error.to_string().contains("sending publication"), "{error}");
        assert_eq!(s, before);
    }
    let api = Fake::new().with(
        "pulls?state=all&head=o%3Afeature&base=main&per_page=100&page=1",
        json!([]),
    );
    // Cancelled task cannot reconcile through mutable(); it also cannot erase uncertainty.
    assert!(pr::reconcile(
        &mut s,
        &json!({"task":"o/r#1", "state_version":5}),
        23.0,
        Some(&api)
    )
    .is_err());
    assert_eq!(s, before);
}

#[test]
fn unconfirmed_publication_retains_writer_exclusion_until_observed() {
    let repo = Repo::new();
    let mut s = prepared(&repo);
    begin(&mut s).unwrap();
    let query = "pulls?state=all&head=o%3Afeature&base=main&per_page=100&page=1";
    let api = Fake::new().with(query, json!([]));
    let r = pr::reconcile(
        &mut s,
        &json!({"task":"o/r#1", "state_version":3}),
        21.0,
        Some(&api),
    )
    .unwrap();
    assert_eq!(r["pr"]["phase"], "publication_unconfirmed");
    assert!(reserve(&s, &repo.root, "other").is_err());
    let pull = pr_json(&repo, false);
    let api = Fake::new()
        .with(query, json!([pull.clone()]))
        .with("pulls/7", pull);
    pr::reconcile(
        &mut s,
        &json!({"task":"o/r#1", "state_version":r["state_version"]}),
        22.0,
        Some(&api),
    )
    .unwrap();
    assert!(reserve(&s, &repo.root, "other").is_ok());
}

#[test]
fn writer_fails_closed_on_unverifiable_sending_ownership() {
    let repo = Repo::new();
    let s = prepared(&repo);
    for bad in [
        json!(false),
        json!({}),
        json!({"publication":{"phase":"unknown"}}),
        json!({"publication":{"phase":"sending"},"checkout":"relative"}),
        json!({"publication":{"phase":"sending"},"checkout":repo.root.join("missing")}),
    ] {
        let mut state = s.clone();
        state["tasks"]["o/r#1"]["pr"] = bad;
        assert!(reserve(&state, &repo.root, "other").is_err());
    }
    let mut state = s.clone();
    state["tasks"]["o/r#1"]["task"] = json!("other");
    assert!(reserve(&state, &repo.root, "other").is_err());
    let mut s = s;
    begin(&mut s).unwrap();
    assert!(reserve(&s, &repo.root.join("missing"), "other").is_err());
    assert!(reserve(&json!({}), &repo.root, "new").is_ok());
    assert!(reserve(&json!({"tasks":false}), &repo.root, "new").is_err());
}

#[test]
fn existing_pr_number_ownership_check_is_preserved() {
    let repo = Repo::new();
    let mut s = prepared(&repo);
    begin(&mut s).unwrap();
    let mut foreign = other(&repo);
    foreign["pr"] = s["tasks"]["o/r#1"]["pr"].clone();
    foreign["pr"]["head_ref"] = json!("another-branch");
    foreign["pr"]["number"] = json!(7);
    s["tasks"]["other"] = foreign;
    let pull = pr_json(&repo, false);
    let api = Fake::new()
        .with(
            "pulls?state=all&head=o%3Afeature&base=main&per_page=100&page=1",
            json!([pull.clone()]),
        )
        .with("pulls/7", pull);
    let before = s.clone();
    let error = pr::reconcile(
        &mut s,
        &json!({"task":"o/r#1", "state_version":3}),
        21.0,
        Some(&api),
    )
    .unwrap_err();
    assert!(error
        .to_string()
        .contains("PR is already owned by another task"));
    assert_eq!(s, before);
}

fn persistent_queue(repo: &Repo, tasks: J) -> PathBuf {
    let guard = repo._tmp.path().join("guard");
    common::create_dir(&guard).unwrap();
    util::atomic_write_json(
        &guard.join("state.json"),
        &json!({
            "version":2,"task":"o/r#1","role":"coordinator","pane":"w1:p1",
            "terminal":"fixture","socket":"/fixture","cwd":repo.root,
            "session":"fixture-session","phase":"monitoring","auto_rotate":false
        }),
    )
    .unwrap();
    let root = repo._tmp.path().join("queue");
    let mut s = queue::init(&root, Some(&guard), 30).unwrap();
    s["tasks"] = tasks;
    queue::save(&root, &mut s).unwrap();
    root
}

fn cli(root: &Path, action: &str, spec: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_herdr-flow"));
    command
        .args([
            "queue",
            action,
            "--state-dir",
            root.to_str().unwrap(),
            "--spec",
            spec.to_str().unwrap(),
            "--json",
        ])
        .env("CODEX_THREAD_ID", "fixture-session")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    command
}

#[test]
fn cli_lock_serializes_competing_intents_and_refusal_preserves_durable_state() {
    let repo = Repo::new();
    let root = persistent_queue(&repo, json!({"o/r#1":task(&repo),"other":other(&repo)}));
    let a = repo._tmp.path().join("a.json");
    let b = repo._tmp.path().join("b.json");
    util::atomic_write_json(&a, &prepare_spec(&repo, 1)).unwrap();
    let mut other_spec = prepare_spec(&repo, 1);
    other_spec["task"] = json!("other");
    util::atomic_write_json(&b, &other_spec).unwrap();
    let before = std::fs::read(root.join("queue.json")).unwrap();
    let lock = queue::locked(&root, false).unwrap();
    let busy = cli(&root, "pr-prepare", &a).output().unwrap();
    assert!(!busy.status.success());
    assert!(String::from_utf8_lossy(&busy.stderr).contains("busy"));
    assert_eq!(std::fs::read(root.join("queue.json")).unwrap(), before);
    drop(lock);

    let first = cli(&root, "pr-prepare", &a).spawn().unwrap();
    let second = cli(&root, "pr-prepare", &b).spawn().unwrap();
    let results = [
        first.wait_with_output().unwrap(),
        second.wait_with_output().unwrap(),
    ];
    assert_eq!(
        results.iter().filter(|r| r.status.success()).count(),
        1,
        "{:?}",
        results
            .iter()
            .map(|r| String::from_utf8_lossy(&r.stderr))
            .collect::<Vec<_>>()
    );
    let loser = if results[0].status.success() { &b } else { &a };
    let before = std::fs::read(root.join("queue.json")).unwrap();
    let rejected = cli(&root, "pr-prepare", loser).output().unwrap();
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("intent is owned by another task"));
    assert_eq!(std::fs::read(root.join("queue.json")).unwrap(), before);
    let state = queue::load_queue(&root).unwrap();
    assert_eq!(
        state["tasks"]
            .as_object()
            .unwrap()
            .values()
            .filter(|t| !t["pr"].is_null())
            .count(),
        1
    );
}

#[test]
fn shared_task_action_and_cli_begin_reject_late_writer_without_saving() {
    let repo = Repo::new();
    let mut s = prepared(&repo);
    s["tasks"]["other"] = other(&repo);
    s["tasks"]["other"]["writer"] = writer(&repo.root, "same-session");
    let root = persistent_queue(&repo, s["tasks"].clone());
    let spec = json!({"task":"o/r#1", "state_version":2, "authorization":"publish"});
    let path = repo._tmp.path().join("begin.json");
    util::atomic_write_json(&path, &spec).unwrap();
    let before = std::fs::read(root.join("queue.json")).unwrap();
    let lock = queue::locked(&root, false).unwrap();
    let error = queue::task_action(&root, "pr-begin", &spec, Some("fixture-session"), &|_| {
        panic!("no receiver or model call is needed")
    })
    .unwrap_err();
    assert!(error.to_string().contains("another task's writer"));
    assert_eq!(std::fs::read(root.join("queue.json")).unwrap(), before);
    drop(lock);
    let output = cli(&root, "pr-begin", &path).output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("another task's writer"));
    assert_eq!(std::fs::read(root.join("queue.json")).unwrap(), before);
}

#[test]
fn own_writer_still_requires_existing_idle_check_at_begin() {
    let repo = Repo::new();
    let mut s = queue_state(task(&repo));
    s["tasks"]["o/r#1"]["writer"] = writer(&repo.root, "same-session");
    pr::prepare(&mut s, &prepare_spec(&repo, 1), 10.0).unwrap();
    let before = s.clone();
    assert!(begin(&mut s)
        .unwrap_err()
        .to_string()
        .contains("inspect writer release"));
    assert_eq!(s, before);
    progress::release(
        &mut s,
        &json!({"task":"o/r#1", "state_version":2,
        "evidence":"inspected current writer release"}),
        21.0,
    )
    .unwrap();
    assert!(begin(&mut s).is_ok());
}
