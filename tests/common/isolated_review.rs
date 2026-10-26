//! Real Git snapshots and isolated ownership; no model or login changes.
use super::*;

fn review_activity(f: &Fixture, env: &task_env::TaskEnvironment) -> String {
    let developer = env.workspace_root.join("checkout");
    fs::write(developer.join("file"), "review candidate\n").unwrap();
    git(&developer, &["add", "file"]);
    git(&developer, &["-c", "user.name=Fixture", "-c", "user.email=fixture@example.test", "commit", "-qm", "candidate"]);
    let commit = git(&developer, &["rev-parse", "HEAD"]);
    let mut q = f.state();
    for mut spec in [
        json!({"kind":"artifact","artifact":{"adapter":"git","id":commit,"location":developer}}),
        json!({"kind":"candidate","candidate":commit,"reason":"fixed review input"}),
        json!({"kind":"activity","activity":"review","role":"acceptance","input":commit,"session_epoch":1,
            "result_wait":{"id":"review","kind":"signal","task":"review","epoch":1,"assignment":"review-a1",
                "owner":"reviewer","reason":"independent review","condition":"report","timeout_seconds":600}}),
    ] {
        spec["task"] = json!("task");
        spec["state_version"] = q["tasks"]["task"]["state_version"].clone();
        progress::lineage::action(&mut q, &spec, util::now()).unwrap();
    }
    queue::save(&f.queue, &mut q).unwrap();
    commit
}

fn snapshot_cli(f: &Fixture, root: &Path, wait: &str) -> i32 {
    let spec = f.temp.path().join("snapshot-spec.json");
    util::atomic_write_json(&spec, &json!({"queue_dir":f.queue,"wait":wait})).unwrap();
    task_runtime::cli::cli(&["review-snapshot".into(), "--task".into(), "task".into(),
        "--state-dir".into(), root.to_string_lossy().into_owned(), "--spec".into(), spec.to_string_lossy().into_owned()])
}

#[test]
fn registered_review_snapshot_is_independent_fixed_and_recoverable() {
    let f = Fixture::new();
    let env = isolated_runtime::linked(&f);
    let commit = review_activity(&f, &env);
    let root = task_runtime::prepare(&env, "review-a1").unwrap();
    let developer = env.workspace_root.join("checkout");
    let snapshot = env.workspace_root.join("reviews/review-a1");
    drop(env);
    assert_eq!(snapshot_cli(&f, &root, "review"), 0);
    let env = task_env::restore_environment("task").unwrap();
    let before = util::read_json(env.state_file()).unwrap();
    assert!(!task_env::paths_overlap(&snapshot, &developer));
    assert_eq!(git(&snapshot, &["rev-parse", "HEAD"]), commit);
    fs::write(developer.join("file"), "developer advances independently\n").unwrap();
    drop(env);
    assert_eq!(snapshot_cli(&f, &root, "review"), 0);
    let env = task_env::restore_environment("task").unwrap();
    assert_eq!(util::read_json(env.state_file()).unwrap(), before);
    assert_eq!(fs::read_to_string(snapshot.join("file")).unwrap(), "review candidate\n");
    assert_eq!(git(&snapshot, &["rev-parse", "HEAD"]), commit);
    fs::write(snapshot.join("file"), "tracked drift\n").unwrap();
    drop(env);
    assert_ne!(snapshot_cli(&f, &root, "review"), 0);
    assert_eq!(fs::read_to_string(snapshot.join("file")).unwrap(), "tracked drift\n");
}

#[test]
fn review_snapshot_and_runtime_are_removed_by_existing_terminal_cleanup() {
    let f = Fixture::new();
    let env = isolated_runtime::linked(&f);
    review_activity(&f, &env);
    let root = task_runtime::prepare(&env, "review-a1").unwrap();
    let workspace = env.workspace_root.clone();
    let temp = env.temp_root.clone();
    drop(env);
    assert_eq!(snapshot_cli(&f, &root, "review"), 0);
    fs::write(workspace.join("reviews/review-a1/untracked-build"), "temporary").unwrap();
    f.finalize("failure").unwrap();
    let report = queue::finalization::drive(&f.queue).unwrap();
    assert_eq!(report.completed, 1, "{:?}", report);
    assert!(!workspace.exists());
    assert!(!temp.exists());
    assert!(f.state()["tasks"]["task"].is_null());
    assert!(f.state()["tasks"]["review"].is_null());
    assert!(!task_env::is_registered("task").unwrap());
    assert!(!task_env::environment_state_path("task").unwrap().exists());
    assert!(!task_env::cleanup_state_path("task").unwrap().exists());
    assert!(f.temp.path().join("repo").is_dir());
}
