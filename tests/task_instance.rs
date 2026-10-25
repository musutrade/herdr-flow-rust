//! PR1 creation boundaries only: real fixture Git/processes, no model or deployment.
use herdr_flow::{
    task_env::{
        self,
        instance::{self, CreateSpec, CreationPhase, InstanceBinding},
        RootIdentity, TaskEnvironment,
    },
    task_runtime,
};
use serde_json::Value;
use std::{
    fs,
    os::unix::{
        fs::{symlink, PermissionsExt},
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::Command,
    sync::{Mutex, MutexGuard},
};
static LOCK: Mutex<()> = Mutex::new(());
struct Fixture {
    dir: tempfile::TempDir,
    old: Option<std::ffi::OsString>,
    _lock: MutexGuard<'static, ()>,
}
impl Fixture {
    fn new() -> Self {
        let lock = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::Builder::new()
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        let old = std::env::var_os("HERDR_TASK_ENV_ROOT");
        std::env::set_var("HERDR_TASK_ENV_ROOT", dir.path().join("control"));
        let f = Self {
            dir,
            old,
            _lock: lock,
        };
        fs::create_dir(f.path("repo")).unwrap();
        fs::set_permissions(f.path("repo"), fs::Permissions::from_mode(0o700)).unwrap();
        git(&f.path("repo"), &["init", "-q"]);
        fs::write(f.path("repo/file"), "formal input\n").unwrap();
        git(&f.path("repo"), &["add", "file"]);
        git(
            &f.path("repo"),
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.test",
                "commit",
                "-qm",
                "input",
            ],
        );
        f
    }
    fn path(&self, p: &str) -> PathBuf {
        self.dir.path().join(p)
    }
    fn binding(&self) -> InstanceBinding {
        InstanceBinding {
            queue_id: "fixture-queue".into(),
            task_id: "task".into(),
            epoch: 7,
        }
    }
    fn spec(&self) -> CreateSpec {
        CreateSpec {
            workspace_root: self.path("work"),
            temp_root: self.path("temp"),
            delivery_targets: vec![],
            repository: self.path("repo"),
            commit: git(&self.path("repo"), &["rev-parse", "HEAD"]),
        }
    }
    fn begin(&self) -> TaskEnvironment {
        instance::begin_instance(&self.binding(), &self.spec()).unwrap()
    }
    fn ready(&self) -> TaskEnvironment {
        let mut e = self.begin();
        instance::materialize_instance(&mut e).unwrap();
        e
    }
    fn resume(&self, id: &str) -> TaskEnvironment {
        instance::resume_instance(&self.binding(), id).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        match &self.old {
            Some(v) => std::env::set_var("HERDR_TASK_ENV_ROOT", v),
            None => std::env::remove_var("HERDR_TASK_ENV_ROOT"),
        }
    }
}
fn git(repo: &Path, args: &[&str]) -> String {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(repo).args(args);
    // SAFETY: child-only scalar syscall, no shared process environment changes.
    unsafe {
        cmd.pre_exec(|| {
            libc::umask(0o077);
            Ok(())
        });
    }
    let o = cmd.output().unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8(o.stdout).unwrap().trim().into()
}
fn mkdir(path: &Path) -> RootIdentity {
    fs::create_dir(path).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    RootIdentity::of(&fs::symlink_metadata(path).unwrap())
}
fn roots_checkpoint(e: &mut TaskEnvironment) {
    e.workspace_identity = Some(mkdir(&e.workspace_root));
    e.persist().unwrap();
    e.temp_identity = Some(mkdir(&e.temp_root));
    e.persist().unwrap();
    let id = mkdir(&e.workspace_root.join("checkout"));
    e.creation.as_mut().unwrap().checkout_identity = Some(id);
    e.persist().unwrap();
}
fn git_checkpoint(e: &mut TaskEnvironment) {
    roots_checkpoint(e);
    e.creation.as_mut().unwrap().git_admin_before = Some(Default::default());
    e.creation.as_mut().unwrap().phase = CreationPhase::GitPending;
    e.persist().unwrap();
    let i = e.creation.as_ref().unwrap();
    git(
        &i.repository,
        &[
            "worktree",
            "add",
            "--detach",
            "--no-checkout",
            i.checkout.to_str().unwrap(),
            &i.commit,
        ],
    );
}
fn document(e: &TaskEnvironment) -> Value {
    serde_json::from_slice(&fs::read(e.state_file()).unwrap()).unwrap()
}

#[test]
fn intent_precedes_roots_and_completed_reply_loss_preserves_exact_instance() {
    let f = Fixture::new();
    let e = f.begin();
    let id = e.instance_id.clone();
    assert_eq!(document(&e)["state"], "creating");
    assert!(!f.path("work").exists());
    assert!(instance::begin_instance(&f.binding(), &f.spec()).is_err());
    assert!(instance::resume_instance(&f.binding(), &id).is_err()); // owned lock
    drop(e);
    let mut e = f.resume(&id);
    instance::materialize_instance(&mut e).unwrap();
    assert_eq!(
        fs::read_to_string(f.path("work/checkout/file")).unwrap(),
        "formal input\n"
    );
    let before = document(&e);
    drop(e);
    let mut e = f.resume(&id);
    instance::materialize_instance(&mut e).unwrap();
    assert_eq!(document(&e), before);
    assert_eq!(e.worktree_registrations.len(), 1);
    assert_eq!(
        task_env::status("task").unwrap()["recovery"],
        "queue_instance_resume_only"
    );
}

#[test]
fn resumes_after_one_root_checkpoint_without_replacing_it() {
    let f = Fixture::new();
    let mut e = f.begin();
    let id = mkdir(&e.workspace_root);
    e.workspace_identity = Some(id);
    e.persist().unwrap();
    let instance = e.instance_id.clone();
    drop(e);
    let mut e = f.resume(&instance);
    instance::materialize_instance(&mut e).unwrap();
    assert_eq!(e.workspace_identity, Some(id));
    instance::verify_owned(&e).unwrap();
}

#[test]
fn successful_git_add_with_lost_reply_is_reconciled_not_duplicated() {
    let f = Fixture::new();
    let mut e = f.begin();
    git_checkpoint(&mut e);
    let before = git(&f.path("repo"), &["worktree", "list", "--porcelain"]);
    let id = e.instance_id.clone();
    drop(e);
    let mut e = f.resume(&id);
    instance::materialize_instance(&mut e).unwrap();
    assert_eq!(
        git(&f.path("repo"), &["worktree", "list", "--porcelain"]),
        before
    );
    assert_eq!(e.worktree_registrations.len(), 1);
    assert!(f.path("work/checkout/file").exists());
}

#[test]
fn missing_git_side_effect_can_resume_the_same_intent() {
    let f = Fixture::new();
    let mut e = f.begin();
    roots_checkpoint(&mut e);
    e.creation.as_mut().unwrap().git_admin_before = Some(Default::default());
    e.creation.as_mut().unwrap().phase = CreationPhase::GitPending;
    e.persist().unwrap();
    instance::materialize_instance(&mut e).unwrap();
    instance::verify_owned(&e).unwrap();
}

#[test]
fn unknown_partial_directory_is_retained_without_adoption_or_deletion() {
    let f = Fixture::new();
    let mut e = f.begin();
    mkdir(&e.workspace_root);
    fs::write(f.path("work/sentinel"), "foreign").unwrap();
    let before = document(&e);
    assert!(instance::materialize_instance(&mut e)
        .unwrap_err()
        .to_string()
        .contains("no recorded identity"));
    assert_eq!(document(&e), before);
    assert_eq!(
        fs::read_to_string(f.path("work/sentinel")).unwrap(),
        "foreign"
    );
}

#[test]
fn replacement_root_checkout_and_git_file_fail_closed() {
    for path in ["work", "work/checkout", "work/checkout/.git"] {
        let f = Fixture::new();
        let e = f.ready();
        let original = f.path(path);
        let backup = f.path("saved");
        fs::rename(&original, &backup).unwrap();
        if path.ends_with(".git") {
            fs::copy(&backup, &original).unwrap();
        } else {
            mkdir(&original);
        }
        let before = document(&e);
        assert!(instance::verify_owned(&e).is_err(), "accepted {path}");
        assert_eq!(document(&e), before);
        assert!(backup.exists());
        assert!(original.exists());
    }
}

#[test]
fn replaced_admin_and_repository_are_not_same_path_authority() {
    for admin in [true, false] {
        let f = Fixture::new();
        let e = f.ready();
        let i = e.creation.as_ref().unwrap();
        let path = if admin {
            i.git_identity.as_ref().unwrap().admin_dir.clone()
        } else {
            i.repository.clone()
        };
        let backup = f.path("saved");
        fs::rename(&path, &backup).unwrap();
        mkdir(&path);
        assert!(instance::verify_owned(&e).is_err());
        assert!(backup.exists());
    }
}

#[test]
fn swapped_symlink_and_foreign_git_backlink_are_rejected() {
    let f = Fixture::new();
    let e = f.ready();
    let i = e.creation.as_ref().unwrap();
    let admin = i.git_identity.as_ref().unwrap().admin_dir.clone();
    fs::write(
        admin.join("gitdir"),
        f.path("foreign/.git").to_str().unwrap(),
    )
    .unwrap();
    assert!(instance::verify_owned(&e).is_err());
    fs::rename(f.path("work"), f.path("saved")).unwrap();
    symlink(f.path("saved"), f.path("work")).unwrap();
    assert!(instance::verify_owned(&e).is_err());
    assert!(f.path("saved/checkout/file").exists());
}

#[test]
fn no_adoption_wrong_epoch_foreign_instance_and_legacy_binding() {
    let f = Fixture::new();
    mkdir(&f.path("work"));
    assert!(instance::begin_instance(&f.binding(), &f.spec()).is_err());
    fs::remove_dir(f.path("work")).unwrap();
    let e = f.ready();
    let id = e.instance_id.clone();
    drop(e);
    let mut b = f.binding();
    b.epoch += 1;
    assert!(instance::resume_instance(&b, &id).is_err());
    assert!(instance::resume_instance(&f.binding(), "replacement").is_err());
    let legacy =
        task_env::create_environment("legacy", &f.path("legacy"), vec![], &f.path("legacy-temp"))
            .unwrap();
    assert!(instance::owner(&legacy).is_none());
    assert!(instance::verify_owned(&legacy).is_err());
}

#[test]
fn interrupted_population_preserves_foreign_files_and_cannot_be_active() {
    let f = Fixture::new();
    let mut e = f.begin();
    git_checkpoint(&mut e);
    fs::write(f.path("work/checkout/file"), "do not overwrite").unwrap();
    fs::write(f.path("work/checkout/untracked"), "preserve").unwrap();
    assert!(instance::materialize_instance(&mut e).is_err());
    assert_eq!(document(&e)["state"], "creating");
    assert_eq!(
        fs::read_to_string(f.path("work/checkout/file")).unwrap(),
        "do not overwrite"
    );
    assert_eq!(
        fs::read_to_string(f.path("work/checkout/untracked")).unwrap(),
        "preserve"
    );
}

#[test]
fn active_instance_allows_own_head_to_advance_not_registration_adoption() {
    let f = Fixture::new();
    let mut e = f.ready();
    let checkout = f.path("work/checkout");
    git(
        &checkout,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.test",
            "commit",
            "--allow-empty",
            "-qm",
            "development",
        ],
    );
    instance::verify_owned(&e).unwrap();
    assert!(e.register_worktree(&f.path("work/other")).is_err());
    e.register_worktree(&checkout).unwrap();
    assert_eq!(e.worktree_registrations.len(), 1);
}

#[test]
fn managed_child_and_runtime_use_same_task_epoch_instance() {
    let f = Fixture::new();
    let mut e = f.ready();
    let root = task_runtime::prepare(&e, "worker").unwrap();
    let mut cmd = task_runtime::command(&e, &root, Path::new("/bin/sleep")).unwrap();
    let envs: std::collections::BTreeMap<_, _> = cmd
        .get_envs()
        .map(|(k, v)| {
            (
                k.to_string_lossy().to_string(),
                v.map(|s| s.to_string_lossy().to_string()),
            )
        })
        .collect();
    assert_eq!(envs["HERDR_TASK_ID"].as_deref(), Some("task"));
    assert_eq!(envs["HERDR_TASK_EPOCH"].as_deref(), Some("7"));
    assert_eq!(
        envs[task_env::MARKER_ENV].as_deref(),
        Some(e.instance_id.as_str())
    );
    let mut child = cmd.arg("30").spawn().unwrap();
    let registered = e.register_process(child.id() as i32);
    child.kill().unwrap();
    child.wait().unwrap();
    registered.unwrap();
    assert_eq!(e.child_processes.len(), 1);
    assert!(e.child_processes[0].start_time.is_some());
}

#[test]
fn bound_instance_cannot_enter_legacy_destroy_or_recover() {
    let f = Fixture::new();
    let e = f.ready();
    let before = document(&e);
    assert!(task_env::destroy_environment(&e).is_err());
    assert_eq!(document(&e), before);
    drop(e);
    assert!(task_env::recover_interrupted("task").is_err());
    assert!(f.path("work/checkout/file").exists());
    assert!(task_env::cleanup_failed_environment("task")
        .unwrap_err()
        .to_string()
        .contains("not enabled"));
    assert!(task_env::resume_delivery_cleanup("task").is_err());
}

#[test]
fn durable_instance_replacement_rejects_a_still_live_handle() {
    let f = Fixture::new();
    let e = f.ready();
    let mut value = document(&e);
    value["instance_id"] = serde_json::json!("new-instance");
    herdr_flow::util::atomic_write_json(e.state_file(), &value).unwrap();
    assert!(instance::verify_owned(&e)
        .unwrap_err()
        .to_string()
        .contains("durable task instance changed"));
    assert!(task_runtime::prepare(&e, "late").is_err());
    assert!(!f.path("temp/rt/late").exists());
}

#[test]
fn registry_checkpoint_missing_is_only_recovered_for_creating() {
    let f = Fixture::new();
    let e = f.begin();
    let id = e.instance_id.clone();
    task_env::unregister_environment("task", &id).unwrap();
    drop(e);
    let mut e = f.resume(&id);
    instance::materialize_instance(&mut e).unwrap();
    task_env::unregister_environment("task", &id).unwrap();
    assert!(instance::verify_owned(&e)
        .unwrap_err()
        .to_string()
        .contains("lost its registry"));
    assert!(f.path("work/checkout/file").exists());
}

#[test]
fn git_recovery_rejects_wrong_head_and_partial_unregistered_content() {
    for wrong_head in [true, false] {
        let f = Fixture::new();
        let mut e = f.begin();
        if wrong_head {
            git_checkpoint(&mut e);
            git(
                &f.path("work/checkout"),
                &[
                    "-c",
                    "user.name=Fixture",
                    "-c",
                    "user.email=fixture@example.test",
                    "commit",
                    "--allow-empty",
                    "-qm",
                    "foreign",
                ],
            );
        } else {
            roots_checkpoint(&mut e);
            e.creation.as_mut().unwrap().git_admin_before = Some(Default::default());
            e.creation.as_mut().unwrap().phase = CreationPhase::GitPending;
            e.persist().unwrap();
            fs::write(f.path("work/checkout/foreign"), "retain").unwrap();
        }
        assert!(instance::materialize_instance(&mut e).is_err());
        assert_eq!(document(&e)["state"], "creating");
    }
}

#[test]
fn invalid_creation_spec_has_no_owned_side_effects() {
    let f = Fixture::new();
    let original = f.spec();
    for which in 0..5 {
        let mut s = original.clone();
        match which {
            0 => s.commit = "HEAD".into(),
            1 => s.repository = PathBuf::from("relative"),
            2 => s.workspace_root = s.repository.clone(),
            3 => s.temp_root = s.workspace_root.clone(),
            _ => s.workspace_root = f.path("repo/.git/new"),
        }
        assert!(instance::begin_instance(&f.binding(), &s).is_err());
        assert!(!f.path("control/envs/task.json").exists());
    }
}

#[test]
fn orphan_administration_after_pending_intent_is_not_replayed() {
    let f = Fixture::new();
    let mut e = f.begin();
    roots_checkpoint(&mut e);
    let i = e.creation.as_mut().unwrap();
    i.git_admin_before = Some(Default::default());
    i.phase = CreationPhase::GitPending;
    e.persist().unwrap();
    mkdir(&f.path("repo/.git/worktrees"));
    mkdir(&f.path("repo/.git/worktrees/orphan"));
    fs::write(f.path("repo/.git/worktrees/orphan/sentinel"), "retain").unwrap();
    assert!(instance::materialize_instance(&mut e)
        .unwrap_err()
        .to_string()
        .contains("no replay"));
    assert!(!f.path("work/checkout/.git").exists());
    assert!(f.path("repo/.git/worktrees/orphan/sentinel").exists());
}

#[test]
fn active_queue_checkpoint_recovery_does_not_repopulate_task_changes() {
    let f = Fixture::new();
    let e = f.ready();
    let id = e.instance_id.clone();
    fs::write(f.path("work/checkout/file"), "task edit").unwrap();
    fs::write(f.path("work/checkout/untracked"), "task temporary data").unwrap();
    drop(e);
    let mut e = f.resume(&id);
    instance::materialize_instance(&mut e).unwrap();
    assert_eq!(
        fs::read_to_string(f.path("work/checkout/file")).unwrap(),
        "task edit"
    );
    assert_eq!(
        fs::read_to_string(f.path("work/checkout/untracked")).unwrap(),
        "task temporary data"
    );
}

#[test]
fn mismatched_registry_claims_fail_even_with_matching_convenience_paths() {
    let f = Fixture::new();
    let e = f.ready();
    let path = task_env::registry_path().unwrap();
    let original: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    for (key, value) in [
        ("roots", serde_json::json!([f.path("foreign")])),
        ("task_id", serde_json::json!("other")),
    ] {
        let mut saved = original.clone();
        saved["entries"]["task"][key] = value;
        herdr_flow::util::atomic_write_json(&path, &saved).unwrap();
        assert!(instance::verify_owned(&e).is_err());
    }
}

#[test]
fn configured_checkout_filter_is_rejected_without_executing_it() {
    let f = Fixture::new();
    let sentinel = f.path("filter-ran");
    git(
        &f.path("repo"),
        &[
            "config",
            "filter.fixture.smudge",
            &format!("touch {}; cat", sentinel.display()),
        ],
    );
    fs::write(f.path("repo/.gitattributes"), "file filter=fixture\n").unwrap();
    git(&f.path("repo"), &["add", ".gitattributes"]);
    git(
        &f.path("repo"),
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.test",
            "commit",
            "-qm",
            "attributes",
        ],
    );
    assert!(instance::begin_instance(&f.binding(), &f.spec())
        .unwrap_err()
        .to_string()
        .contains("filters are unsupported"));
    assert!(!sentinel.exists());
    assert!(!f.path("work").exists());
}

#[test]
fn live_creation_execution_prevents_replay_then_same_intent_resumes() {
    let f = Fixture::new();
    let mut e = f.begin();
    let mut child = Command::new("/bin/sleep")
        .arg("30")
        .env(task_env::MARKER_ENV, &e.instance_id)
        .spawn()
        .unwrap();
    // Wait for exec to expose the marker rather than testing the fork window.
    for _ in 0..200 {
        if task_env::process_has_marker(child.id() as i32, &e.instance_id) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let before = document(&e);
    let refusal = instance::materialize_instance(&mut e);
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(refusal
        .unwrap_err()
        .to_string()
        .contains("live owned execution"));
    assert_eq!(document(&e), before);
    assert!(!f.path("work").exists());
    instance::materialize_instance(&mut e).unwrap();
}

#[test]
#[ignore = "subprocess-only supervisor fixture, not a model"]
fn instance_git_supervisor_worker() {
    let pid_file = std::env::var("INSTANCE_PID_FILE").unwrap();
    let options = herdr_flow::util::process::CommandOptions {
        kill_on_parent_death: true,
        timeout: std::time::Duration::from_secs(30),
        ..Default::default()
    };
    let _ = herdr_flow::util::process::run_command(
        &[
            "/bin/sh".into(),
            "-c".into(),
            "echo $$ > \"$1\"; exec sleep 30".into(),
            "fixture".into(),
            pid_file,
        ],
        &options,
    );
}

#[test]
fn direct_creation_child_dies_with_its_supervisor_before_fixture_drop() {
    let f = Fixture::new();
    let pid_file = f.path("pid");
    let mut supervisor = Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "instance_git_supervisor_worker",
            "--nocapture",
        ])
        .env("INSTANCE_PID_FILE", &pid_file)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let mut pid = None;
    for _ in 0..400 {
        pid = fs::read_to_string(&pid_file)
            .ok()
            .and_then(|s| s.trim().parse::<i32>().ok());
        if pid.is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    supervisor.kill().unwrap();
    supervisor.wait().unwrap();
    let pid = pid.expect("supervisor did not create child");
    let record = task_env::ProcessRecord {
        pid,
        start_time: task_env::process_start_time(pid),
    };
    for _ in 0..400 {
        if !task_env::process_record_is_alive(&record) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(
        !task_env::process_record_is_alive(&record),
        "owned direct child survived supervisor death"
    );
}
