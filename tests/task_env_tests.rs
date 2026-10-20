//! Integration tests for task environment lifecycle and its boundaries.
//!
//! Each test uses an isolated `HERDR_TASK_ENV_ROOT`, so state never touches the
//! real `~/.herdr` tree and tests can run in parallel.

use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Barrier};

use herdr_flow::task_env::*;
use tempfile::TempDir;

/// The registry root is process-global, so tests that point it somewhere else
/// take this lock for their whole duration.
static HARNESS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Isolated environment harness: owns a temp dir and points the registry at it.
struct Harness {
    root: TempDir,
    _guard: std::sync::MutexGuard<'static, ()>,
}

impl Harness {
    fn new() -> Self {
        let guard = HARNESS_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let root = tempfile::Builder::new()
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        std::env::set_var("HERDR_TASK_ENV_ROOT", root.path().join("state"));
        Self {
            root,
            _guard: guard,
        }
    }

    fn path(&self) -> &Path {
        self.root.path()
    }

    fn workspace(&self, name: &str) -> PathBuf {
        self.path().join(name).join("workspace")
    }

    fn temp(&self, name: &str) -> PathBuf {
        self.path().join(name).join("temp")
    }

    fn create(&self, task_id: &str) -> TaskEnvironment {
        create_environment(
            task_id,
            &self.workspace(task_id),
            vec![],
            &self.temp(task_id),
        )
        .unwrap()
    }

    fn create_with_targets(
        &self,
        task_id: &str,
        targets: Vec<DeliveryTarget>,
    ) -> herdr_flow::Result<TaskEnvironment> {
        create_environment(
            task_id,
            &self.workspace(task_id),
            targets,
            &self.temp(task_id),
        )
    }
}

fn delivery(path: PathBuf) -> DeliveryTarget {
    DeliveryTarget::new(path, "file".to_string())
}

#[test]
fn create_and_destroy_round_trip() {
    let harness = Harness::new();
    let env = harness.create("round-trip");
    assert!(env.workspace_root.is_dir());
    assert_eq!(env.state, EnvironmentState::Active);
    assert!(env.state_file().exists());
    assert!(is_registered("round-trip").unwrap());

    destroy_environment(&env).unwrap();
    assert!(!env.workspace_root.exists());
    assert!(!env.temp_root.exists());
    assert!(!env.state_file().exists());
    assert!(!is_registered("round-trip").unwrap());
}

#[test]
fn restore_returns_the_same_environment_and_never_duplicates_it() {
    let harness = Harness::new();
    let first = harness.create("restore");
    let restored = restore_environment("restore").unwrap();
    assert_eq!(first.task_id, restored.task_id);
    assert_eq!(first.workspace_root, restored.workspace_root);
    assert_eq!(first.created_at, restored.created_at);

    // A second create must be rejected rather than allocating another root.
    let duplicate = harness.create_with_targets("restore", vec![]);
    assert!(duplicate.is_err());

    destroy_environment(&restored).unwrap();
}

#[test]
fn delivery_targets_survive_destroy() {
    let harness = Harness::new();
    let delivered = harness.path().join("deliverable.txt");
    fs::write(&delivered, "formal delivery").unwrap();

    let env = harness
        .create_with_targets("delivery", vec![delivery(delivered.clone())])
        .unwrap();
    destroy_environment(&env).unwrap();

    assert_eq!(fs::read_to_string(&delivered).unwrap(), "formal delivery");
}

#[test]
fn symlink_alias_cannot_hide_a_delivery_target_inside_the_workspace() {
    let harness = Harness::new();
    let workspace = harness.workspace("alias");
    fs::create_dir_all(&workspace).unwrap();
    let alias = harness.path().join("alias-link");
    symlink(&workspace, &alias).unwrap();
    let smuggled = alias.join("delivered.txt");
    fs::write(&smuggled, "formal delivery").unwrap();

    let result = create_environment(
        "alias",
        &workspace,
        vec![delivery(smuggled.clone())],
        &harness.temp("alias"),
    );
    // Either the create is rejected, or the resolved target is recognised as
    // being inside the workspace and rejected; it must never be accepted.
    assert!(
        result.is_err(),
        "symlink alias defeated boundary validation"
    );
    assert!(smuggled.exists());
}

#[test]
fn preexisting_user_assets_are_never_destroyed() {
    let harness = Harness::new();
    let workspace = harness.workspace("assets");
    fs::create_dir_all(&workspace).unwrap();
    let asset = workspace.join("user-data");
    fs::write(&asset, "existing user data").unwrap();

    let result = create_environment("assets", &workspace, vec![], &harness.temp("assets"));
    assert!(result.is_err(), "claimed a non-empty directory");
    assert!(asset.exists());
}

#[test]
fn delivery_inside_workspace_or_temp_is_rejected() {
    let harness = Harness::new();
    let workspace = harness.workspace("inside-workspace");
    let err = harness
        .create_with_targets("inside-workspace", vec![delivery(workspace.join("out"))])
        .unwrap_err();
    assert!(err.to_string().contains("inside disposable root"));
    assert!(!workspace.exists());

    let temp = harness.temp("inside-temp");
    let err = harness
        .create_with_targets("inside-temp", vec![delivery(temp.join("out"))])
        .unwrap_err();
    assert!(err.to_string().contains("inside disposable root"));
}

#[test]
fn protected_file_permissions_are_not_changed_by_cleanup() {
    let harness = Harness::new();
    let protected = harness.path().join("protected.txt");
    fs::write(&protected, "protected").unwrap();
    fs::set_permissions(&protected, fs::Permissions::from_mode(0o400)).unwrap();

    let env = harness
        .create_with_targets("chmod", vec![delivery(protected.clone())])
        .unwrap();
    // A symlink inside the workspace pointing at the protected file must not
    // let the writable walk escape.
    symlink(&protected, env.workspace_root.join("link")).unwrap();
    destroy_environment(&env).unwrap();

    let mode = fs::metadata(&protected).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o400);
}

#[test]
fn forged_gitdir_cannot_delete_a_protected_directory() {
    let harness = Harness::new();
    let protected = harness.path().join("protected-dir");
    fs::create_dir(&protected).unwrap();
    fs::write(protected.join("delivered"), "formal delivery").unwrap();

    let mut env = harness
        .create_with_targets("gitdir", vec![delivery(protected.clone())])
        .unwrap();
    let worktree = env.workspace_root.join("worktree");
    fs::create_dir(&worktree).unwrap();
    fs::write(
        worktree.join(".git"),
        format!("gitdir: {}\n", protected.display()),
    )
    .unwrap();
    env.register_worktree(&worktree).unwrap();

    destroy_environment(&env).unwrap();
    assert!(protected.join("delivered").exists());
}

#[test]
fn registered_open_file_is_released_on_destroy() {
    let harness = Harness::new();
    let mut env = harness.create("open-file");
    let path = env.workspace_root.join("open.txt");
    fs::write(&path, b"payload").unwrap();
    env.register_file(&path).unwrap();
    assert_eq!(env.open_handle_count(), 1);

    destroy_environment(&env).unwrap();

    // The environment's own descriptor must be closed and the path removed.
    // A descriptor opened independently by the caller cannot be revoked from
    // another owner, so this asserts the environment's ownership only.
    assert_eq!(
        env.open_handle_count(),
        0,
        "environment leaked its descriptor"
    );
    assert!(!path.exists(), "registered file survived destroy");
}

#[test]
fn destroy_terminates_registered_process_and_its_descendants() {
    let harness = Harness::new();
    let mut env = harness.create("descendants");
    let pidfile = harness.path().join("child.pid");
    let mut child = Command::new("sh")
        .arg("-c")
        .arg("sleep 60 & echo $! > \"$1\"; wait")
        .arg("herdr-test")
        .arg(&pidfile)
        .spawn()
        .unwrap();

    for _ in 0..500 {
        if fs::read_to_string(&pidfile)
            .map(|text| !text.trim().is_empty())
            .unwrap_or(false)
        {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let descendant: i32 = fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    env.register_process(child.id() as i32).unwrap();

    destroy_environment(&env).unwrap();
    let _ = child.wait();
    // SAFETY: kill(pid, 0) only probes liveness for a positive pid we spawned.
    let alive = unsafe { libc::kill(descendant, 0) == 0 };
    if alive {
        // SAFETY: best-effort cleanup of the isolated descendant we spawned.
        unsafe { libc::kill(descendant, libc::SIGKILL) };
    }
    assert!(!alive, "background descendant survived destroy");
}

#[test]
fn non_positive_pids_are_rejected() {
    let harness = Harness::new();
    let mut env = harness.create("pid-guard");
    assert!(env.register_process(-1).is_err());
    assert!(env.register_process(0).is_err());
    assert!(env.register_process(1).is_err());
    destroy_environment(&env).unwrap();
}

#[test]
fn failed_destroy_keeps_the_slot_and_retry_completes_it() {
    let harness = Harness::new();
    let env = harness.create("retry");
    // Replace the workspace with a regular file so removal fails.
    fs::remove_dir_all(&env.workspace_root).unwrap();
    fs::write(&env.workspace_root, "force ENOTDIR").unwrap();

    assert!(destroy_environment(&env).is_err());
    assert!(
        is_registered("retry").unwrap(),
        "slot was released on failure"
    );
    assert!(occupied_slots().unwrap() >= 1);
    let restored = restore_environment("retry").unwrap();
    assert_eq!(restored.state, EnvironmentState::DestroyFailed);
    fs::remove_file(&env.workspace_root).unwrap();

    cleanup_failed_environment("retry").unwrap();
    assert!(!env.state_file().exists(), "retry left the state document");
    assert!(!is_registered("retry").unwrap());
    assert!(cleanup_state_path("retry")
        .map(|p| !p.exists())
        .unwrap_or(false));
}

#[test]
fn repeatedly_failing_cleanup_exhausts_its_retries_instead_of_allocating_again() {
    let harness = Harness::new();
    std::env::set_var("HERDR_TASK_ENV_LIMIT", "1");
    let env = harness.create("stuck");
    fs::remove_dir_all(&env.workspace_root).unwrap();
    fs::write(&env.workspace_root, "force ENOTDIR").unwrap();
    // Make the directory unremovable for the duration: a non-empty file tree
    // is enough because remove_dir_all on a file fails.
    assert!(destroy_environment(&env).is_err());

    // Slot is still occupied, so the limit blocks another environment.
    let blocked = harness.create_with_targets("other", vec![]);
    assert!(
        blocked.is_err(),
        "limit did not account for the failed cleanup"
    );

    for _ in 0..MAX_ATTEMPTS {
        let _ = cleanup_failed_environment("stuck");
    }
    let exhausted = cleanup_failed_environment("stuck").unwrap_err();
    assert!(exhausted.to_string().contains("already failed"));

    fs::remove_file(&env.workspace_root).unwrap();
    std::env::remove_var("HERDR_TASK_ENV_LIMIT");
}

#[test]
fn interrupted_create_leaves_no_untracked_workspace() {
    let harness = Harness::new();
    let blocked = harness.path().join("blocked");
    fs::write(&blocked, "not a directory").unwrap();

    let result = create_environment(
        "interrupted",
        &harness.workspace("interrupted"),
        vec![],
        &blocked.join("temp"),
    );
    assert!(result.is_err());
    assert!(!harness.workspace("interrupted").exists());
    assert!(!is_registered("interrupted").unwrap());
    assert!(!environment_state_path("interrupted")
        .map(|p| p.exists())
        .unwrap_or(false));
}

#[test]
fn concurrent_registrations_do_not_lose_updates() {
    let harness = Harness::new();
    let seed = harness.create("seed");
    let barrier = Arc::new(Barrier::new(16));
    let mut handles = Vec::new();
    for index in 0..16 {
        let barrier = Arc::clone(&barrier);
        let state = harness.path().join(format!("registered-{index}.json"));
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            register_environment(&format!("concurrent-{index}"), &state)
        }));
    }
    for handle in handles {
        handle.join().unwrap().unwrap();
    }
    let registry = EnvironmentRegistry::load().unwrap();
    for index in 0..16 {
        assert!(
            registry.contains(&format!("concurrent-{index}")),
            "concurrent registration {index} was overwritten"
        );
    }
    destroy_environment(&seed).unwrap();
}

#[test]
fn task_id_cannot_escape_the_state_directory() {
    let harness = Harness::new();
    for bad in ["../escape", "a/b", ".", "..", ""] {
        let result = create_environment(
            bad,
            &harness.path().join("ws-escape"),
            vec![],
            &harness.path().join("tmp-escape"),
        );
        assert!(result.is_err(), "accepted traversal task id {bad:?}");
    }
    assert!(!harness.path().join("escape.json").exists());
}

#[test]
fn unmanaged_paths_are_rejected_by_validate_path() {
    let harness = Harness::new();
    let delivered = harness.path().join("delivery.txt");
    fs::write(&delivered, "x").unwrap();
    let env = harness
        .create_with_targets("boundaries", vec![delivery(delivered.clone())])
        .unwrap();

    assert!(validate_path(&env, &env.workspace_root.join("work.txt")).is_ok());
    assert!(validate_path(&env, &env.temp_root.join("cache")).is_ok());
    assert!(validate_path(&env, &delivered).is_ok());
    assert!(validate_path(&env, &harness.path().join("outside.txt")).is_err());
    assert!(is_within_disposable(&env, &env.workspace_root.join("a")));
    assert!(!is_within_disposable(&env, &delivered));

    destroy_environment(&env).unwrap();
}

#[test]
fn destroy_is_idempotent() {
    let harness = Harness::new();
    let env = harness.create("idempotent");
    destroy_environment(&env).unwrap();
    // A second destroy finds no plan and no directory: it must succeed.
    destroy_environment(&env).unwrap();
}
