//! Integration tests for task environment lifecycle and its boundaries.
//!
//! The registry root is process-global, so every test holds one mutex and
//! points `HERDR_TASK_ENV_ROOT` at an isolated temporary directory.

use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Barrier, MutexGuard};

use herdr_flow::task_env::*;
use tempfile::TempDir;

static HARNESS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct Harness {
    root: TempDir,
    _guard: MutexGuard<'static, ()>,
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
        std::env::remove_var("HERDR_TASK_ENV_LIMIT");
        Self {
            root,
            _guard: guard,
        }
    }

    fn path(&self) -> &Path {
        self.root.path()
    }

    fn workspace(&self, task_id: &str) -> PathBuf {
        self.path().join(task_id).join("workspace")
    }

    fn temp(&self, task_id: &str) -> PathBuf {
        self.path().join(task_id).join("temp")
    }

    /// Create the parent directory the roots live in (the environment never
    /// creates unowned ancestors).
    fn prepare(&self, task_id: &str) {
        let dir = self.path().join(task_id);
        fs::create_dir_all(&dir).unwrap();
        // Keep ancestors private regardless of the process umask; creation
        // rejects group/world-writable ancestors by design.
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
    }

    fn create(&self, task_id: &str) -> TaskEnvironment {
        self.prepare(task_id);
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
        self.prepare(task_id);
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

/// Spawn a shell owned by `env` (session leader + ownership marker).
fn spawn_detached(env: &TaskEnvironment, script: &str, arg: &Path) -> std::process::Child {
    env.command("sh")
        .arg("-c")
        .arg(script)
        .arg("herdr-test")
        .arg(arg)
        .spawn()
        .unwrap()
}

fn wait_for_pid_file(path: &Path) -> i32 {
    for _ in 0..1000 {
        if let Ok(text) = fs::read_to_string(path) {
            if let Ok(pid) = text.trim().parse() {
                return pid;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    panic!("pid file {} was never written", path.display());
}

fn alive(pid: i32) -> bool {
    // A zombie has already released its resources; it only waits to be reaped.
    let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    let state = stat
        .rsplit_once(')')
        .and_then(|(_, rest)| rest.split_whitespace().next())
        .and_then(|state| state.chars().next());
    !matches!(state, None | Some('Z'))
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
fn restore_refuses_a_second_owner_and_reopens_after_release() {
    let harness = Harness::new();
    let env = harness.create("restore");
    // The live handle owns the environment.
    assert!(restore_environment("restore").is_err());

    destroy_environment(&env).unwrap();
    assert!(restore_environment("restore").is_err());
    // A second create with the same id is allowed once the first is gone.
    let second = harness.create("restore");
    destroy_environment(&second).unwrap();
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
fn delivery_containing_a_root_is_rejected() {
    let harness = Harness::new();
    let workspace = harness.workspace("nested");
    fs::create_dir_all(workspace.parent().unwrap()).unwrap();
    // Delivery target is an ancestor of the workspace.
    let err = create_environment(
        "nested",
        &workspace,
        vec![delivery(workspace.parent().unwrap().to_path_buf())],
        &harness.temp("nested"),
    )
    .unwrap_err();
    assert!(err.to_string().contains("overlaps"));
    assert!(!workspace.exists());
}

#[test]
fn symlink_alias_cannot_hide_a_delivery_target() {
    let harness = Harness::new();
    let workspace = harness.workspace("alias");
    fs::create_dir_all(&workspace).unwrap();
    let alias = harness.path().join("alias-link");
    symlink(&workspace, &alias).unwrap();
    let smuggled = alias.join("delivered.txt");
    fs::write(&smuggled, "formal delivery").unwrap();

    harness.prepare("alias");
    let result = create_environment(
        "alias",
        &workspace,
        vec![delivery(smuggled.clone())],
        &harness.temp("alias"),
    );
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
fn one_empty_workspace_cannot_be_claimed_twice() {
    let harness = Harness::new();
    harness.prepare("shared");
    let shared = harness.workspace("shared");
    fs::create_dir(&shared).unwrap();
    fs::set_permissions(&shared, fs::Permissions::from_mode(0o700)).unwrap();

    let first = {
        harness.prepare("first");
        create_environment("first", &shared, vec![], &harness.temp("first")).unwrap()
    };

    harness.prepare("second");
    let second = create_environment("second", &shared, vec![], &harness.temp("second"));
    assert!(second.is_err(), "two environments claimed the same root");

    fs::write(shared.join("second-data"), "belongs to first").unwrap();
    destroy_environment(&first).unwrap();
    // First owned the root, so its destroy removes it; the second environment
    // never existed and left no trace.
    assert!(!shared.exists());
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
    // Symlink and hard link inside the workspace both point at the delivery.
    symlink(&protected, env.workspace_root.join("link")).unwrap();
    fs::hard_link(&protected, env.workspace_root.join("hard")).unwrap();
    destroy_environment(&env).unwrap();

    let mode = fs::metadata(&protected).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o400, "cleanup changed the delivery file mode");
    assert!(protected.exists());
}

#[test]
fn swapped_root_symlink_cannot_delete_the_delivery_directory() {
    let harness = Harness::new();
    let delivered = harness.path().join("delivered-dir");
    fs::create_dir(&delivered).unwrap();
    fs::write(delivered.join("payload"), "formal delivery").unwrap();

    let env = harness
        .create_with_targets("swap-root", vec![delivery(delivered.clone())])
        .unwrap();
    let workspace = env.workspace_root.clone();
    fs::remove_dir(&workspace).unwrap();
    symlink(&delivered, &workspace).unwrap();

    let result = destroy_environment(&env);
    assert!(result.is_err(), "destroy followed a swapped root symlink");
    assert_eq!(
        fs::read_to_string(delivered.join("payload")).unwrap(),
        "formal delivery"
    );
    // The slot stays occupied while the root is unsafe.
    assert!(is_registered("swap-root").unwrap());
}

#[test]
fn swapped_file_symlink_removes_only_the_link() {
    let harness = Harness::new();
    let delivered = harness.path().join("delivery-file");
    fs::write(&delivered, "formal delivery").unwrap();

    let mut env = harness
        .create_with_targets("swap-file", vec![delivery(delivered.clone())])
        .unwrap();
    let registered = env.workspace_root.join("registered");
    fs::write(&registered, "task data").unwrap();
    env.register_file(&registered).unwrap();
    fs::remove_file(&registered).unwrap();
    symlink(&delivered, &registered).unwrap();

    destroy_environment(&env).unwrap();
    assert_eq!(fs::read_to_string(&delivered).unwrap(), "formal delivery");
    assert!(!registered.exists(), "the link itself should be removed");
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
fn real_git_worktree_registration_is_removed() {
    let harness = Harness::new();
    let repo = harness.path().join("repo");
    fs::create_dir(&repo).unwrap();
    let git = |args: &[&str]| {
        let status = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "test@example.com"]);
    git(&["config", "user.name", "Test"]);
    fs::write(repo.join("file"), "base").unwrap();
    git(&["add", "."]);
    git(&["commit", "-qm", "base"]);

    let mut env = harness.create("worktree");
    let worktree = env.workspace_root.join("linked");
    let status = Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["worktree", "add", "-q", "-b", "linked"])
        .arg(&worktree)
        .status()
        .unwrap();
    assert!(status.success());
    env.register_worktree(&worktree).unwrap();

    destroy_environment(&env).unwrap();

    let output = Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .unwrap();
    let listing = String::from_utf8_lossy(&output.stdout);
    assert!(
        !listing.contains(worktree.to_string_lossy().as_ref()),
        "worktree registration survived destroy: {listing}"
    );
}

#[test]
fn registered_file_handle_is_closed_by_destroy() {
    let harness = Harness::new();
    let mut env = harness.create("open-file");
    let path = env.workspace_root.join("open.txt");
    let handle = fs::File::create(&path).unwrap();
    env.register_handle(&path, handle).unwrap();
    assert_eq!(env.open_handle_count(), 1);

    destroy_environment(&env).unwrap();
    assert_eq!(env.open_handle_count(), 0);
    assert!(!path.exists());
}

#[test]
fn destroying_a_stale_handle_cannot_touch_a_new_instance() {
    let harness = Harness::new();
    let stale = harness.create("reused");
    destroy_environment(&stale).unwrap();

    let current = harness.create("reused");
    let result = destroy_environment(&stale);
    assert!(result.is_err(), "stale handle acted on the new environment");
    assert!(current.workspace_root.exists());
    assert!(current.state_file().exists());
    assert!(is_registered("reused").unwrap());

    destroy_environment(&current).unwrap();
}

#[test]
fn destroy_terminates_the_whole_registered_process_group() {
    let harness = Harness::new();
    let mut env = harness.create("descendants");
    let pidfile = harness.path().join("child.pid");

    // The parent stays alive just long enough to be registered, then exits and
    // leaves a background child behind in the same process group.
    let mut parent = spawn_detached(
        &env,
        "sleep 60 & echo $! > \"$1\"; sleep 0.3; exit 0",
        &pidfile,
    );
    let descendant = wait_for_pid_file(&pidfile);
    env.register_process(parent.id() as i32).unwrap();
    let _ = parent.wait();

    destroy_environment(&env).unwrap();
    let _ = parent.wait();
    let still_alive = alive(descendant);
    if still_alive {
        // SAFETY: best-effort cleanup of the isolated descendant we spawned.
        unsafe { libc::kill(descendant, libc::SIGKILL) };
    }
    assert!(!still_alive, "background descendant survived destroy");
}

#[test]
fn sigterm_handler_cannot_spawn_a_survivor() {
    let harness = Harness::new();
    let mut env = harness.create("sigterm-child");
    let pidfile = harness.path().join("trap-child.pid");

    let mut parent = spawn_detached(
        &env,
        "trap 'sleep 60 & echo $! > \"$1\"' TERM; while :; do sleep 0.05; done",
        &pidfile,
    );
    env.register_process(parent.id() as i32).unwrap();

    // Destruction freezes the process group before signalling, so the TERM
    // handler either never runs or any child it did spawn is killed in the
    // same pass. Both outcomes are acceptable; a survivor is not.
    destroy_environment(&env).unwrap();
    let _ = parent.wait();
    if let Ok(text) = fs::read_to_string(&pidfile) {
        if let Ok(child) = text.trim().parse::<i32>() {
            let still_alive = alive(child);
            if still_alive {
                // SAFETY: best-effort cleanup of the isolated child we spawned.
                unsafe { libc::kill(child, libc::SIGKILL) };
            }
            assert!(
                !still_alive,
                "child spawned during SIGTERM survived destroy"
            );
        }
    }
}

#[test]
fn descendant_that_left_the_group_is_killed_by_the_marker() {
    let harness = Harness::new();
    let mut env = harness.create("setsid-orphan");
    let pidfile = harness.path().join("setsid.pid");

    // The parent starts a child in a new session, then exits. The marker is
    // inherited by the child and survives the orphaned state.
    let mut parent = spawn_detached(
        &env,
        "setsid sleep 60 & echo $! > \"$1\"; sleep 0.3; exit 0",
        &pidfile,
    );
    let descendant = wait_for_pid_file(&pidfile);
    env.register_process(parent.id() as i32).unwrap();
    let _ = parent.wait();

    destroy_environment(&env).unwrap();
    let still_alive = alive(descendant);
    if still_alive {
        // SAFETY: best-effort cleanup of the isolated descendant we spawned.
        unsafe { libc::kill(descendant, libc::SIGKILL) };
    }
    assert!(!still_alive, "setsid descendant survived destroy");
}

#[test]
fn descendant_spawned_after_registration_is_still_marked() {
    let harness = Harness::new();
    let mut env = harness.create("late-orphan");
    let pidfile = harness.path().join("late.pid");
    let go = harness.path().join("late.pid.go");

    // Register first, then let the parent spawn a detached child and exit.
    let mut parent = spawn_detached(
        &env,
        "while [ ! -f \"$1.go\" ]; do sleep 0.01; done; setsid sleep 60 & echo $! > \"$1\"; exit 0",
        &pidfile,
    );
    env.register_process(parent.id() as i32).unwrap();
    fs::write(&go, "go").unwrap();
    let descendant = wait_for_pid_file(&pidfile);
    let _ = parent.wait();

    destroy_environment(&env).unwrap();
    let still_alive = alive(descendant);
    if still_alive {
        // SAFETY: best-effort cleanup of the isolated descendant we spawned.
        unsafe { libc::kill(descendant, libc::SIGKILL) };
    }
    assert!(
        !still_alive,
        "descendant spawned after registration survived destroy"
    );
}

#[test]
fn plan_only_recovery_is_discovered_and_completed() {
    let harness = Harness::new();
    let mut env = harness.create("plan-only");
    let file = env.workspace_root.join("owned.txt");
    fs::write(&file, "task data").unwrap();
    env.register_file(&file).unwrap();
    fs::remove_file(&file).unwrap();
    fs::create_dir(&file).unwrap();
    assert!(destroy_environment(&env).is_err());
    assert!(cleanup_state_path("plan-only").unwrap().exists());

    // Simulate a crash after the document was removed but before the plan was.
    fs::remove_file(env.state_file()).unwrap();
    drop(env);

    assert!(interrupted_tasks()
        .unwrap()
        .contains(&"plan-only".to_string()));
    assert!(recover_interrupted("plan-only").unwrap());
    assert!(!cleanup_state_path("plan-only").unwrap().exists());
    assert_eq!(occupied_slots().unwrap(), 0);
}

#[test]
fn unrelated_supervisor_child_survives_destroy() {
    let harness = Harness::new();
    // Not started through the environment, so it carries no marker.
    let mut unrelated = Command::new("sleep").arg("30").spawn().unwrap();
    let pid = unrelated.id() as i32;
    let env = harness.create("unrelated");

    destroy_environment(&env).unwrap();
    assert!(alive(pid), "destroy killed an unrelated supervisor child");
    let _ = unrelated.kill();
    let _ = unrelated.wait();
}

#[test]
fn destroying_one_environment_leaves_anothers_orphan_alone() {
    let harness = Harness::new();
    let first = harness.create("first-env");
    let mut second = harness.create("second-env");
    let pidfile = harness.path().join("second-orphan.pid");

    let mut parent = spawn_detached(
        &second,
        "setsid sleep 60 & echo $! > \"$1\"; sleep 0.3; exit 0",
        &pidfile,
    );
    let orphan = wait_for_pid_file(&pidfile);
    second.register_process(parent.id() as i32).unwrap();
    let _ = parent.wait();

    destroy_environment(&first).unwrap();
    assert!(alive(orphan), "destroying A killed B's marked orphan");

    destroy_environment(&second).unwrap();
    assert!(!alive(orphan), "B's own destroy left its orphan alive");
}

#[test]
fn another_environments_registration_cannot_hide_an_orphan() {
    let harness = Harness::new();
    let mut first = harness.create("hidden-orphan");
    let pidfile = harness.path().join("hidden.pid");
    let mut parent = spawn_detached(
        &first,
        "setsid sleep 60 & echo $! > \"$1\"; sleep 0.3; exit 0",
        &pidfile,
    );
    let orphan = wait_for_pid_file(&pidfile);
    first.register_process(parent.id() as i32).unwrap();
    let _ = parent.wait();
    // Register a second environment after the orphan exists.
    let second = harness.create("hiding-env");

    destroy_environment(&first).unwrap();
    assert!(!alive(orphan), "A's orphan survived A's destroy");
    destroy_environment(&second).unwrap();
}

#[test]
fn mismatched_process_identity_is_never_signalled() {
    let harness = Harness::new();
    let mut unrelated = Command::new("sleep").arg("30").spawn().unwrap();
    let pid = unrelated.id() as i32;
    let env = harness.create("identity");

    // Create a plan, then inject a record that claims the unrelated pid with a
    // stale start time, simulating pid reuse.
    fs::remove_dir_all(&env.workspace_root).unwrap();
    fs::write(&env.workspace_root, "force failure").unwrap();
    assert!(destroy_environment(&env).is_err());
    let plan_path = cleanup_state_path("identity").unwrap();
    let mut plan: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&plan_path).unwrap()).unwrap();
    plan["pending_processes"] = serde_json::json!([{
        "pid": pid,
        "pgid": pid,
        "start_time": 1,
    }]);
    plan["quarantine"] = serde_json::json!({
        "root": env.workspace_root,
        "name": ".herdr-rm-test",
        "unlink_committed": true,
    });
    fs::write(&plan_path, serde_json::to_string_pretty(&plan).unwrap()).unwrap();
    fs::remove_file(&env.workspace_root).unwrap();
    drop(env);

    cleanup_failed_environment("identity").unwrap();
    assert!(alive(pid), "a mismatched identity was signalled");
    let _ = unrelated.kill();
    let _ = unrelated.wait();
}

#[test]
fn claiming_two_roots_keeps_both_claims() {
    let harness = Harness::new();
    harness.prepare("claim");
    let a = harness.path().join("claim").join("a");
    let b = harness.path().join("claim").join("b");
    for dir in [&a, &b] {
        fs::create_dir(dir).unwrap();
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).unwrap();
    }

    let claimed =
        claim_roots_exclusively("claim-task", "instance-a", &[a.clone(), b.clone()]).unwrap();
    assert_eq!(claimed.len(), 2);
    let registry = load_registry().unwrap();
    let entry = registry.get("claim-task").expect("claim entry missing");
    assert_eq!(entry.claimed_roots().len(), 2);

    // A different instance can claim neither root.
    assert!(claim_roots_exclusively("other-task", "instance-b", &[a])
        .unwrap()
        .is_empty());
    assert!(claim_roots_exclusively("other-task", "instance-b", &[b])
        .unwrap()
        .is_empty());
}

#[test]
fn processes_outside_their_own_group_are_rejected() {
    let harness = Harness::new();
    let mut env = harness.create("pid-guard");
    assert!(env.register_process(-1).is_err());
    assert!(env.register_process(0).is_err());
    assert!(env.register_process(1).is_err());
    // A plain child shares the test process group and must be refused.
    let mut child = Command::new("sleep").arg("30").spawn().unwrap();
    assert!(env.register_process(child.id() as i32).is_err());
    let _ = child.kill();
    let _ = child.wait();
    destroy_environment(&env).unwrap();
}

#[test]
fn failed_destroy_keeps_the_slot_and_retry_completes_it() {
    let harness = Harness::new();
    let mut env = harness.create("retry");
    let file = env.workspace_root.join("owned.txt");
    fs::write(&file, "task data").unwrap();
    env.register_file(&file).unwrap();
    // A directory where a registered file used to be makes that step fail.
    fs::remove_file(&file).unwrap();
    fs::create_dir(&file).unwrap();

    assert!(destroy_environment(&env).is_err());
    assert!(is_registered("retry").unwrap());
    assert!(occupied_slots().unwrap() >= 1);
    let restored = restore_environment("retry").unwrap();
    assert_eq!(restored.state, EnvironmentState::DestroyFailed);
    drop(restored);

    // The root pass still removed the workspace, so the pending file is simply
    // gone; the retry must finish as an idempotent no-op.
    cleanup_failed_environment("retry").unwrap();
    assert!(!env.state_file().exists());
    assert!(!is_registered("retry").unwrap());
    assert!(!cleanup_state_path("retry").unwrap().exists());
}

#[test]
fn failed_cleanup_exhausts_retries_and_keeps_the_slot() {
    let harness = Harness::new();
    std::env::set_var("HERDR_TASK_ENV_LIMIT", "1");
    let mut env = harness.create("stuck");
    let file = env.workspace_root.join("owned.txt");
    fs::write(&file, "task data").unwrap();
    env.register_file(&file).unwrap();
    fs::remove_file(&file).unwrap();
    fs::create_dir(&file).unwrap();
    assert!(destroy_environment(&env).is_err());

    harness.prepare("other");
    let blocked = create_environment(
        "other",
        &harness.workspace("other"),
        vec![],
        &harness.temp("other"),
    );
    assert!(blocked.is_err(), "limit ignored the pending cleanup");

    // Drive the plan to the retry bound and confirm it refuses to continue.
    let plan_path = cleanup_state_path("stuck").unwrap();
    let mut plan: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&plan_path).unwrap()).unwrap();
    plan["attempts"] = serde_json::json!(MAX_ATTEMPTS);
    fs::write(&plan_path, serde_json::to_string_pretty(&plan).unwrap()).unwrap();
    let exhausted = cleanup_failed_environment("stuck").unwrap_err();
    assert!(exhausted.to_string().contains("already failed"));

    let _ = file;
    std::env::remove_var("HERDR_TASK_ENV_LIMIT");
}

#[test]
fn interrupted_create_leaves_no_untracked_workspace() {
    let harness = Harness::new();
    let blocked = harness.path().join("blocked");
    fs::write(&blocked, "not a directory").unwrap();

    harness.prepare("interrupted");
    let result = create_environment(
        "interrupted",
        &harness.workspace("interrupted"),
        vec![],
        &blocked.join("temp"),
    );
    assert!(result.is_err());
    assert!(!harness.workspace("interrupted").exists());
    assert!(!is_registered("interrupted").unwrap());
    assert!(!environment_state_path("interrupted").unwrap().exists());
}

#[test]
fn interrupted_state_is_detected_and_recovered() {
    let harness = Harness::new();
    let env = harness.create("recover");
    // Simulate a crash after the document was written but before the
    // environment became usable.
    let text = fs::read_to_string(env.state_file()).unwrap();
    let mut value: serde_json::Value = serde_json::from_str(&text).unwrap();
    value["state"] = serde_json::json!("creating");
    fs::write(
        env.state_file(),
        serde_json::to_string_pretty(&value).unwrap(),
    )
    .unwrap();
    drop(env);

    assert_eq!(interrupted_tasks().unwrap(), vec!["recover".to_string()]);
    assert!(recover_interrupted("recover").unwrap());
    assert!(!environment_state_path("recover").unwrap().exists());
    assert!(!is_registered("recover").unwrap());
}

#[test]
fn recovery_of_an_unclaimed_intent_cannot_delete_another_task_root() {
    let harness = Harness::new();
    let owner = harness.create("owner");
    let intent = harness.create("intent");

    // Simulate a crash after the intent was written but before the root was
    // claimed: point the intent document at the owner's live workspace.
    let text = fs::read_to_string(intent.state_file()).unwrap();
    let mut value: serde_json::Value = serde_json::from_str(&text).unwrap();
    value["state"] = serde_json::json!("creating");
    value["workspace_root"] = serde_json::json!(owner.workspace_root);
    value["workspace_identity"] = serde_json::Value::Null;
    value["temp_root"] = serde_json::json!(harness.temp("intent"));
    value["temp_identity"] = serde_json::Value::Null;
    fs::write(
        intent.state_file(),
        serde_json::to_string_pretty(&value).unwrap(),
    )
    .unwrap();
    let asset = owner.workspace_root.join("owner-data");
    fs::write(&asset, "owned by a live environment").unwrap();
    drop(intent);

    recover_interrupted("intent").unwrap();
    assert!(asset.exists(), "recovery deleted another live task root");
    assert!(is_registered("owner").unwrap());
    assert!(!environment_state_path("intent").unwrap().exists());

    destroy_environment(&owner).unwrap();
}

#[test]
fn destroying_state_is_recovered() {
    let harness = Harness::new();
    let env = harness.create("destroying-state");
    let text = fs::read_to_string(env.state_file()).unwrap();
    let mut value: serde_json::Value = serde_json::from_str(&text).unwrap();
    value["state"] = serde_json::json!("destroying");
    fs::write(
        env.state_file(),
        serde_json::to_string_pretty(&value).unwrap(),
    )
    .unwrap();
    let state_file = env.state_file().to_path_buf();
    drop(env);

    assert!(interrupted_tasks()
        .unwrap()
        .contains(&"destroying-state".to_string()));
    assert!(recover_interrupted("destroying-state").unwrap());
    assert!(!state_file.exists());
    assert!(!is_registered("destroying-state").unwrap());
}

#[test]
fn retry_after_a_committed_root_removal_completes() {
    let harness = Harness::new();
    let env = harness.create("gone-root");
    let workspace = env.workspace_root.clone();
    // Fail at the root step so the root stays pending in the plan.
    fs::remove_dir_all(&workspace).unwrap();
    fs::write(&workspace, "force failure").unwrap();
    assert!(destroy_environment(&env).is_err());
    assert!(cleanup_state_path("gone-root").unwrap().exists());

    // Simulate a crash after the quarantine unlink was committed and executed.
    fs::remove_file(&workspace).unwrap();
    let plan_path = cleanup_state_path("gone-root").unwrap();
    let mut plan: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&plan_path).unwrap()).unwrap();
    plan["quarantine"] = serde_json::json!({
        "root": workspace,
        "name": ".herdr-rm-gone",
        "unlink_committed": true,
    });
    fs::write(&plan_path, serde_json::to_string_pretty(&plan).unwrap()).unwrap();
    drop(env);

    cleanup_failed_environment("gone-root").unwrap();
    assert!(!environment_state_path("gone-root").unwrap().exists());
    assert!(!is_registered("gone-root").unwrap());
    assert!(!plan_path.exists());
}

#[test]
fn renamed_root_without_replacement_retains_the_slot() {
    let harness = Harness::new();
    let env = harness.create("renamed-root");
    let moved = harness.path().join("renamed-away");
    fs::rename(&env.workspace_root, &moved).unwrap();

    assert!(destroy_environment(&env).is_err());
    assert!(is_registered("renamed-root").unwrap());
    assert!(env.state_file().exists());
    assert!(cleanup_state_path("renamed-root").unwrap().exists());
    assert!(moved.exists(), "the renamed root must not be deleted");
}

fn write_root_plan(env: &TaskEnvironment, quarantine: Option<Quarantine>, roots: Vec<PathBuf>) {
    let plan = CleanupState {
        task_id: env.task_id.clone(),
        instance_id: env.instance_id.clone(),
        roots: vec![
            PendingDirectory {
                path: env.workspace_root.clone(),
                identity: env.workspace_identity,
            },
            PendingDirectory {
                path: env.temp_root.clone(),
                identity: env.temp_identity,
            },
        ],
        pending_processes: Vec::new(),
        pending_worktrees: Vec::new(),
        pending_files: Vec::new(),
        pending_root_removals: roots,
        quarantine,
        started_at: 0.0,
        updated_at: 0.0,
        attempts: 0,
        last_error: None,
    };
    fs::write(
        cleanup_state_path(&env.task_id).unwrap(),
        serde_json::to_string_pretty(&plan).unwrap(),
    )
    .unwrap();
}

#[test]
fn crash_before_quarantine_rename_is_retried_from_the_root() {
    let harness = Harness::new();
    let env = harness.create("pre-rename");
    let roots = vec![env.workspace_root.clone()];
    write_root_plan(
        &env,
        Some(Quarantine {
            root: env.workspace_root.clone(),
            name: ".herdr-rm-never-ran".to_string(),
            unlink_committed: false,
        }),
        roots,
    );
    drop(env);

    cleanup_failed_environment("pre-rename").unwrap();
    assert!(!harness.workspace("pre-rename").exists());
    assert!(!is_registered("pre-rename").unwrap());
}

#[test]
fn crash_after_quarantine_rename_completes_on_retry() {
    let harness = Harness::new();
    let env = harness.create("post-rename");
    let quarantine = format!(".herdr-rm-{}", "renamed");
    fs::rename(
        &env.workspace_root,
        harness.path().join("post-rename").join(&quarantine),
    )
    .unwrap();
    write_root_plan(
        &env,
        Some(Quarantine {
            root: env.workspace_root.clone(),
            name: quarantine.clone(),
            unlink_committed: false,
        }),
        vec![env.workspace_root.clone()],
    );
    drop(env);

    cleanup_failed_environment("post-rename").unwrap();
    assert!(!harness
        .path()
        .join("post-rename")
        .join(&quarantine)
        .exists());
    assert!(!is_registered("post-rename").unwrap());
}

#[test]
fn replacement_at_quarantine_is_preserved_and_keeps_the_slot() {
    let harness = Harness::new();
    let env = harness.create("quarantine-swap");
    let parent = harness.path().join("quarantine-swap");
    let quarantine = ".herdr-rm-swapped".to_string();
    fs::rename(&env.workspace_root, parent.join(&quarantine)).unwrap();
    fs::rename(parent.join(&quarantine), parent.join("original")).unwrap();
    fs::create_dir(parent.join(&quarantine)).unwrap();
    write_root_plan(
        &env,
        Some(Quarantine {
            root: env.workspace_root.clone(),
            name: quarantine.clone(),
            unlink_committed: false,
        }),
        vec![env.workspace_root.clone()],
    );
    drop(env);

    assert!(cleanup_failed_environment("quarantine-swap").is_err());
    assert!(parent.join(&quarantine).exists(), "replacement was deleted");
    assert!(parent.join("original").exists());
    assert!(is_registered("quarantine-swap").unwrap());
    assert!(cleanup_state_path("quarantine-swap").unwrap().exists());
}

#[test]
fn task_process_cannot_rewrite_entries_outside_its_roots() {
    let harness = Harness::new();
    let env = harness.create("landlock");
    let outside = harness.path().join("outside.txt");
    fs::write(&outside, "user asset").unwrap();

    let remove = env
        .command("sh")
        .arg("-c")
        .arg("rm -f \"$1\"")
        .arg("herdr-test")
        .arg(&outside)
        .status()
        .unwrap();
    assert!(!remove.success(), "task removed a file outside its roots");

    let moved = harness.path().join("moved-root");
    let rename = env
        .command("sh")
        .arg("-c")
        .arg("mv \"$1\" \"$2\"")
        .arg("herdr-test")
        .arg(&env.workspace_root)
        .arg(&moved)
        .status()
        .unwrap();
    assert!(!rename.success(), "task renamed its own root");
    assert!(outside.exists());
    assert!(!moved.exists());

    destroy_environment(&env).unwrap();
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
        let root = harness.path().join(format!("concurrent-{index}"));
        fs::create_dir(&root).unwrap();
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            register_environment(RegistryEntry {
                task_id: format!("concurrent-{index}"),
                instance_id: format!("instance-{index}"),
                workspace_root: root.join("workspace"),
                temp_root: root.join("temp"),
                roots: vec![root.join("workspace"), root.join("temp")],
                state_file: state,
                registered_at: 0.0,
            })
        }));
    }
    for handle in handles {
        handle.join().unwrap().unwrap();
    }
    let registry = EnvironmentRegistry::load().unwrap();
    for index in 0..16 {
        assert!(registry.contains(&format!("concurrent-{index}")));
    }
    destroy_environment(&seed).unwrap();
}

#[test]
fn task_id_cannot_escape_or_shadow_internal_files() {
    let harness = Harness::new();
    for bad in ["../escape", "a/b", ".", "..", ""] {
        harness.prepare("escape");
        let result = create_environment(
            bad,
            &harness.path().join("ws-escape"),
            vec![],
            &harness.path().join("tmp-escape"),
        );
        assert!(result.is_err(), "accepted traversal task id {bad:?}");
    }
    assert!(!harness.path().join("escape.json").exists());

    // A task called "registry" must not collide with internal files.
    let env = harness.create("registry");
    assert!(EnvironmentRegistry::load().is_ok());
    destroy_environment(&env).unwrap();
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
    destroy_environment(&env).unwrap();
}
