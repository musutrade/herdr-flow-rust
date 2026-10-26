//! Real Git snapshots and isolated ownership; no model or login changes.
use super::*;

fn review_activity(f: &Fixture, env: &task_env::TaskEnvironment) -> String {
    let developer = env.workspace_root.join("checkout");
    fs::write(developer.join("file"), "review candidate\n").unwrap();
    git(&developer, &["add", "file"]);
    git(
        &developer,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.test",
            "commit",
            "-qm",
            "candidate",
        ],
    );
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
    task_runtime::cli::cli(&[
        "review-snapshot".into(),
        "--task".into(),
        "task".into(),
        "--state-dir".into(),
        root.to_string_lossy().into_owned(),
        "--spec".into(),
        spec.to_string_lossy().into_owned(),
    ])
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
    assert_eq!(
        fs::read_to_string(snapshot.join("file")).unwrap(),
        "review candidate\n"
    );
    assert_eq!(git(&snapshot, &["rev-parse", "HEAD"]), commit);
    fs::write(snapshot.join("file"), "tracked drift\n").unwrap();
    drop(env);
    assert_ne!(snapshot_cli(&f, &root, "review"), 0);
    assert_eq!(
        fs::read_to_string(snapshot.join("file")).unwrap(),
        "tracked drift\n"
    );
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
    let mut env = task_env::restore_environment("task").unwrap();
    let snapshot = workspace.join("reviews/review-a1");
    let (runtime, mut child) =
        isolated_runtime::runtime_at(&mut env, "review-a1", &snapshot, Some("review-a1"));
    let spec = json!({"queue_dir":f.queue,"wait":"review","instruction":"synthetic review only"});
    let created = task_runtime::native::create(&env, &runtime, &spec).unwrap();
    assert_eq!(created["cwd"], json!(snapshot));
    assert_eq!(
        task_runtime::native::recover(&env, &runtime).unwrap()["session"],
        created["session"]
    );
    let original = util::read_json(&runtime.join("runtime.json")).unwrap();
    let mut changed = original.clone();
    changed.as_object_mut().unwrap().remove("review_snapshot");
    util::atomic_write_json(&runtime.join("runtime.json"), &changed).unwrap();
    assert!(task_runtime::native::recover(&env, &runtime).is_err());
    util::atomic_write_json(&runtime.join("runtime.json"), &original).unwrap();
    drop(env);
    fs::write(
        workspace.join("reviews/review-a1/untracked-build"),
        "temporary",
    )
    .unwrap();
    f.finalize("failure").unwrap();
    let report = queue::finalization::drive(&f.queue).unwrap();
    assert_eq!(report.completed, 1, "{:?}", report);
    assert!(child.0.try_wait().unwrap().is_some());
    assert!(!workspace.exists());
    assert!(!temp.exists());
    assert!(f.state()["tasks"]["task"].is_null());
    assert!(f.state()["tasks"]["review"].is_null());
    assert!(!task_env::is_registered("task").unwrap());
    assert!(!task_env::environment_state_path("task").unwrap().exists());
    assert!(!task_env::cleanup_state_path("task").unwrap().exists());
    assert!(f.temp.path().join("repo").is_dir());
}

#[test]
fn review_registration_refuses_arbitrary_cwd_foreign_wait_and_existing_directory() {
    let f = Fixture::new();
    let mut env = isolated_runtime::linked(&f);
    review_activity(&f, &env);
    let root = task_runtime::prepare(&env, "review-a1").unwrap();
    let path = env.workspace_root.join("reviews/review-a1");
    let spec = json!({"adapter":"opencode","binary":"/bin/true","cwd":path,"port":1,
        "model":{"providerID":"openai","id":"fixture"},"review_snapshot":"review-a1"});
    assert!(task_runtime::launch(&mut env, &root, &spec).is_err());
    assert!(!root.join("runtime.json").exists());
    let request = json!({"queue_dir":f.queue,"wait":"dev"});
    assert!(task_runtime::review::prepare(&mut env, &root, &request).is_err());
    let mut request = json!({"queue_dir":f.queue,"wait":"review"});
    request["cwd"] = json!(env.workspace_root.join("checkout"));
    assert!(task_runtime::review::prepare(&mut env, &root, &request).is_err());
    util::ensure_private_dir(&path, true).unwrap();
    fs::write(path.join("sentinel"), "not adopted").unwrap();
    assert!(task_runtime::review::prepare(
        &mut env,
        &root,
        &json!({"queue_dir":f.queue,"wait":"review"})
    )
    .is_err());
    assert!(env.review_workspaces.is_empty());
    assert_eq!(
        fs::read_to_string(path.join("sentinel")).unwrap(),
        "not adopted"
    );
}

#[test]
fn snapshot_recovery_reconciles_completed_git_but_never_replays_unknown_clone() {
    use task_env::review::Phase;
    let f = Fixture::new();
    let mut env = isolated_runtime::linked(&f);
    let commit = review_activity(&f, &env);
    let root = task_runtime::prepare(&env, "review-a1").unwrap();
    let request = json!({"queue_dir":f.queue,"wait":"review"});
    task_runtime::review::prepare(&mut env, &root, &request).unwrap();
    let path = env.review_workspaces["review-a1"].path.clone();
    let git_dir = path.join(".git");
    let git_id = task_env::RootIdentity::of(&fs::metadata(&git_dir).unwrap());
    // Synthetic lost-checkpoint states over an actual completed Git clone.
    for phase in [Phase::Cloning, Phase::CheckingOut] {
        env.review_workspaces.get_mut("review-a1").unwrap().phase = phase;
        env.persist().unwrap();
        task_runtime::review::prepare(&mut env, &root, &request).unwrap();
        assert_eq!(env.review_workspaces["review-a1"].phase, Phase::Ready);
        assert_eq!(
            task_env::RootIdentity::of(&fs::metadata(&git_dir).unwrap()),
            git_id
        );
        assert_eq!(git(&path, &["rev-parse", "HEAD"]), commit);
    }
    let displaced = env.workspace_root.join("displaced-git");
    fs::rename(&git_dir, &displaced).unwrap();
    env.review_workspaces.get_mut("review-a1").unwrap().phase = Phase::Cloning;
    env.persist().unwrap();
    assert!(task_runtime::review::prepare(&mut env, &root, &request).is_err());
    assert!(!git_dir.exists(), "unknown clone must not be repeated");
    assert!(displaced.is_dir());
}

#[test]
fn registered_snapshot_refuses_path_replacement_active_writer_and_stale_activity() {
    let f = Fixture::new();
    let mut env = isolated_runtime::linked(&f);
    review_activity(&f, &env);
    let root = task_runtime::prepare(&env, "review-a1").unwrap();
    let request = json!({"queue_dir":f.queue,"wait":"review"});
    task_runtime::review::prepare(&mut env, &root, &request).unwrap();
    let path = env.review_workspaces["review-a1"].path.clone();
    let displaced = env.workspace_root.join("original-review");
    fs::rename(&path, &displaced).unwrap();
    fs::create_dir(&path).unwrap();
    fs::write(path.join("sentinel"), "foreign").unwrap();
    assert!(task_runtime::review::prepare(&mut env, &root, &request).is_err());
    assert_eq!(
        fs::read_to_string(path.join("sentinel")).unwrap(),
        "foreign"
    );
    fs::remove_file(path.join("sentinel")).unwrap();
    fs::remove_dir(&path).unwrap();
    fs::rename(&displaced, &path).unwrap();
    let mut q = f.state();
    q["tasks"]["dev"]["writer"] = json!({"workspace":path});
    queue::save(&f.queue, &mut q).unwrap();
    assert!(task_runtime::review::prepare(&mut env, &root, &request).is_err());
    q["tasks"]["dev"]["writer"] = J::Null;
    q["tasks"]["review"]["control"] = json!("cancelled");
    queue::save(&f.queue, &mut q).unwrap();
    assert!(task_runtime::review::prepare(&mut env, &root, &request).is_err());
}

#[test]
fn review_selection_never_borrows_a_developer_runtime_or_foreign_owner() {
    let f = Fixture::new();
    let mut env = isolated_runtime::linked(&f);
    review_activity(&f, &env);
    let root = task_runtime::prepare(&env, "review-a1").unwrap();
    let request = json!({"queue_dir":f.queue,"wait":"review"});
    task_runtime::review::prepare(&mut env, &root, &request).unwrap();
    let path = env.review_workspaces["review-a1"].path.clone();
    let base = json!({"adapter":"opencode","binary":"/bin/true","port":1,
        "model":{"providerID":"openai","id":"fixture"},"cwd":path,"review_snapshot":"review-a1"});
    for (key, value) in [
        ("cwd", json!(env.workspace_root.join("checkout"))),
        ("review_snapshot", json!("../review-a1")),
        ("review_snapshot", J::Null),
    ] {
        let mut spec = base.clone();
        spec[key] = value;
        assert!(task_runtime::launch(&mut env, &root, &spec).is_err());
        assert!(!root.join("runtime.json").exists());
    }
    let owner = env.review_workspaces["review-a1"].owner.clone();
    env.review_workspaces
        .get_mut("review-a1")
        .unwrap()
        .owner
        .instance_id = "foreign".into();
    env.persist().unwrap();
    assert!(task_runtime::launch(&mut env, &root, &base).is_err());
    env.review_workspaces.get_mut("review-a1").unwrap().owner = owner;
    env.persist().unwrap();
    let mut q = f.state();
    q["tasks"]["review"]["assignment"] = json!("replacement");
    queue::save(&f.queue, &mut q).unwrap();
    assert!(task_runtime::review::prepare(&mut env, &root, &request).is_err());
}
