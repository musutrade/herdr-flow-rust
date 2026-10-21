//! Synthetic stable-base validation for the test-only resource inventory.
#[path = "common/resource_inventory.rs"]
mod resource_inventory;

use herdr_flow::{queue, task_env::*, task_runtime};
use resource_inventory::{collect, InventorySpec};
use serde_json::{json, Value as J};
use std::{
    ffi::OsString,
    fs,
    os::unix::fs::{symlink, DirBuilderExt, PermissionsExt},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Mutex, MutexGuard},
    time::{Duration, Instant},
};
use tempfile::TempDir;

static LOCK: Mutex<()> = Mutex::new(());

struct RegistryRootGuard {
    previous: Option<OsString>,
    _lock: MutexGuard<'static, ()>,
}
impl RegistryRootGuard {
    fn set(path: &Path) -> Self {
        let lock = LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let previous = std::env::var_os("HERDR_TASK_ENV_ROOT");
        std::env::set_var("HERDR_TASK_ENV_ROOT", path);
        Self {
            previous,
            _lock: lock,
        }
    }
}
impl Drop for RegistryRootGuard {
    fn drop(&mut self) {
        match &self.previous {
            Some(value) => std::env::set_var("HERDR_TASK_ENV_ROOT", value),
            None => std::env::remove_var("HERDR_TASK_ENV_ROOT"),
        }
    }
}

struct EnvGuard {
    env: TaskEnvironment,
    child: Option<Child>,
}
impl EnvGuard {
    fn new(env: TaskEnvironment) -> Self {
        Self { env, child: None }
    }
}
impl std::ops::Deref for EnvGuard {
    type Target = TaskEnvironment;
    fn deref(&self) -> &TaskEnvironment {
        &self.env
    }
}
impl std::ops::DerefMut for EnvGuard {
    fn deref_mut(&mut self) -> &mut TaskEnvironment {
        &mut self.env
    }
}
impl Drop for EnvGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if is_registered(&self.env.task_id).unwrap_or(false) {
            if let Err(error) = destroy_environment(&self.env) {
                eprintln!("inventory fixture cleanup failed: {error}");
            }
        }
    }
}

fn temp() -> TempDir {
    tempfile::Builder::new()
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir()
        .unwrap()
}

fn private_dir(path: &Path) {
    fs::DirBuilder::new()
        .mode(0o700)
        .recursive(true)
        .create(path)
        .unwrap();
}

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().into()
}

fn setup_repo(repo: &Path) {
    private_dir(repo);
    git(repo, &["init", "-q"]);
    git(repo, &["config", "user.email", "inventory@example.invalid"]);
    git(repo, &["config", "user.name", "Inventory fixture"]);
    fs::write(repo.join("file"), "base").unwrap();
    git(repo, &["add", "file"]);
    git(repo, &["commit", "-qm", "base"]);
}

fn create_worktree(env: &mut TaskEnvironment, repo: &Path, round: usize) -> PathBuf {
    let worktree = env.workspace_root.join(format!("linked-{round}"));
    git(
        repo,
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            worktree.to_str().unwrap(),
        ],
    );
    env.register_worktree(&worktree).unwrap();
    worktree
}

fn ensure_queue_maps(state: &mut J) {
    for section in ["tasks", "waits", "events", "deliveries"] {
        if !state[section].is_object() {
            state[section] = json!({});
        }
    }
}

fn add_unrelated_queue(q: &Path) {
    let mut state = queue::load_queue(q).unwrap();
    ensure_queue_maps(&mut state);
    state["tasks"]["synthetic-unrelated"] = json!({"task":"synthetic-unrelated","keep":true});
    state["waits"]["synthetic-unrelated-wait"] =
        json!({"id":"synthetic-unrelated-wait","task":"synthetic-unrelated"});
    state["events"]["synthetic-unrelated-event"] =
        json!({"id":"synthetic-unrelated-event","task":"synthetic-unrelated"});
    state["deliveries"]["synthetic-unrelated-delivery"] = json!({
        "id":"synthetic-unrelated-delivery",
        "events":["synthetic-unrelated-event"]
    });
    queue::save(q, &mut state).unwrap();
}

fn add_owned_queue(q: &Path, task: &str) {
    let mut state = queue::load_queue(q).unwrap();
    ensure_queue_maps(&mut state);
    state["tasks"][task] = json!({"task":task});
    let wait = format!("{task}-wait");
    let event = format!("{task}-event");
    let delivery = format!("{task}-delivery");
    state["waits"][wait.as_str()] = json!({"id":wait,"task":task});
    state["events"][event.as_str()] = json!({"id":event,"task":task});
    state["deliveries"][delivery.as_str()] = json!({"id":delivery,"events":[event]});
    queue::save(q, &mut state).unwrap();
}

// Fixture reset only: this is not production task cleanup.
fn reset_owned_queue(q: &Path, task: &str) {
    let mut state = queue::load_queue(q).unwrap();
    for (section, key) in [
        ("tasks", task.to_string()),
        ("waits", format!("{task}-wait")),
        ("events", format!("{task}-event")),
        ("deliveries", format!("{task}-delivery")),
    ] {
        if let Some(map) = state[section].as_object_mut() {
            map.remove(&key);
        }
    }
    queue::save(q, &mut state).unwrap();
}

fn unrelated_values(q: &Path) -> J {
    let state = queue::load_queue(q).unwrap();
    json!({
        "tasks": state["tasks"]["synthetic-unrelated"].clone(),
        "waits": state["waits"]["synthetic-unrelated-wait"].clone(),
        "events": state["events"]["synthetic-unrelated-event"].clone(),
        "deliveries": state["deliveries"]["synthetic-unrelated-delivery"].clone()
    })
}

fn normalized_queue(q: &Path) -> J {
    let mut state = queue::load_queue(q).unwrap();
    if let Some(state) = state.as_object_mut() {
        state.remove("updated_at");
    }
    state
}

fn find_fd(report: &J, outside: &Path) -> Option<(bool, String)> {
    report["processes"]["records"]
        .as_array()?
        .iter()
        .find_map(|record| {
            record["fds"].as_array()?.iter().find_map(|fd| {
                let target = fd["target"].as_str()?;
                if Path::new(target) == outside {
                    Some((
                        record["complete"] == true,
                        fd["class"].as_str().unwrap_or_default().to_string(),
                    ))
                } else {
                    None
                }
            })
        })
}

fn page_keys(report: &J, section: &str) -> Vec<String> {
    report[section]["keys"]
        .as_array()
        .unwrap()
        .iter()
        .map(|key| key.as_str().unwrap().to_string())
        .collect()
}

fn assert_page(report: &J, section: &str, total: usize, keys: &[&str], truncated: bool) {
    assert_eq!(report[section]["total"], total);
    assert_eq!(report[section]["truncated"], truncated);
    assert_eq!(page_keys(report, section), keys);
}

fn assert_owned_queue(report: &J, task: &str) {
    let owned = &report["queue_entries"]["owned"];
    assert_page(owned, "tasks", 1, &[task], false);
    assert_page(owned, "waits", 1, &[&format!("{task}-wait")], false);
    assert_page(owned, "events", 1, &[&format!("{task}-event")], false);
    assert_page(
        owned,
        "deliveries",
        1,
        &[&format!("{task}-delivery")],
        false,
    );
}

fn assert_unrelated_queue(report: &J) {
    let unrelated = &report["queue_entries"]["unrelated"];
    assert_page(unrelated, "tasks", 1, &["synthetic-unrelated"], false);
    assert_page(unrelated, "waits", 1, &["synthetic-unrelated-wait"], false);
    assert_page(
        unrelated,
        "events",
        1,
        &["synthetic-unrelated-event"],
        false,
    );
    assert_page(
        unrelated,
        "deliveries",
        1,
        &["synthetic-unrelated-delivery"],
        false,
    );
}

#[test]
fn stable_base_inventory_collects_and_preserves_ownership_across_two_rounds() {
    let temp = temp();
    let campaign = temp.path().join("campaign");
    private_dir(&campaign);
    let base = campaign.join("inventory");
    private_dir(&base);
    let queue = campaign.join("queue");
    queue::init(&queue, None, 30).unwrap();
    let registry_root = campaign.join("registry");
    let envs_dir = registry_root.join("envs");
    let plans_dir = registry_root.join("plans");
    let repo = campaign.join("repo");
    setup_repo(&repo);
    let outside_dir = campaign.join("outside-dir");
    private_dir(&outside_dir);
    fs::write(outside_dir.join("secret"), "outside").unwrap();
    let outside = campaign.join("outside-fd.txt");
    fs::write(&outside, "outside open fd").unwrap();
    let linked_outside = base.join("linked-outside");
    symlink(&outside_dir, &linked_outside).unwrap();
    let formal = base.join("formal.txt");
    let user = base.join("user.txt");
    let selected = base.join("selected-config.json");
    fs::write(&formal, "formal output").unwrap();
    fs::write(&user, "user asset").unwrap();
    fs::write(&selected, "{}").unwrap();
    let _registry_root = RegistryRootGuard::set(&campaign.join("registry"));
    let excluded = vec![formal.clone(), user.clone(), selected.clone()];
    let base_mode = fs::metadata(&base).unwrap().permissions().mode();
    add_unrelated_queue(&queue);
    let queue_baseline = normalized_queue(&queue);
    let unrelated_baseline = unrelated_values(&queue);
    let mut base_after_first = J::Null;

    for round in 1..=2 {
        let task = format!("synthetic-inventory-{round}");
        let workspace = base.join("workspace");
        let temporary = base.join("temporary");
        let mut env = EnvGuard::new(
            create_environment(
                &task,
                &workspace,
                vec![DeliveryTarget::new(formal.clone(), "file".into())],
                &temporary,
            )
            .unwrap(),
        );
        let instance = env.instance_id.clone();
        let runtime = task_runtime::prepare(&env, "inventory-run").unwrap();
        fs::write(runtime.join("cache/marker"), "cache").unwrap();
        let open = env.workspace_root.join("open.txt");
        let handle = fs::File::create(&open).unwrap();
        env.register_handle(&open, handle).unwrap();
        let worktree = create_worktree(&mut env, &repo, round);
        add_owned_queue(&queue, &task);
        let queue_before = fs::read(queue.join("queue.json")).unwrap();
        let child = env
            .command("/bin/sh")
            .args(["-c", "exec 9<\"$1\"; sleep 30", "sh"])
            .arg(&outside)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let child_id = child.id();
        env.child = Some(child);
        env.register_process(child_id as i32).unwrap();
        let worktrees_before = env.worktree_registrations.clone();
        let open_files_before = env.open_files.clone();
        let runtime_roots = vec![runtime.clone()];

        // Make collector mutation visible: private-dir helpers would chmod these.
        let registry_bytes = fs::read(registry_root.join("registry.json")).unwrap();
        let state_bytes = fs::read(envs_dir.join(format!("{task}.json"))).unwrap();
        fs::set_permissions(&registry_root, fs::Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions(&envs_dir, fs::Permissions::from_mode(0o755)).unwrap();
        if plans_dir.exists() {
            fs::set_permissions(&plans_dir, fs::Permissions::from_mode(0o755)).unwrap();
        }

        let deadline = Instant::now() + Duration::from_secs(3);
        let before = loop {
            let report = collect(&InventorySpec {
                base: &base,
                queue: &queue,
                registry_root: &registry_root,
                envs_dir: &envs_dir,
                plans_dir: &plans_dir,
                task_id: &task,
                instance_id: &instance,
                workspace_root: &workspace,
                temp_root: &temporary,
                runtime_roots: &runtime_roots,
                worktrees: &env.worktree_registrations,
                open_files: &env.open_files,
                excluded: &excluded,
            });
            if let Some((complete, class)) = find_fd(&report, &outside) {
                assert!(complete, "pid/start/fd sampling was incomplete");
                assert_eq!(class, "outside_unknown");
                break report;
            }
            assert!(Instant::now() < deadline, "outside fd was not sampled");
            std::thread::sleep(Duration::from_millis(50));
        };
        assert_eq!(fs::read(queue.join("queue.json")).unwrap(), queue_before);
        assert_eq!(fs::metadata(&base).unwrap().permissions().mode(), base_mode);
        assert_eq!(
            fs::metadata(&registry_root).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert_eq!(
            fs::metadata(&envs_dir).unwrap().permissions().mode() & 0o777,
            0o755
        );
        if plans_dir.exists() {
            assert_eq!(
                fs::metadata(&plans_dir).unwrap().permissions().mode() & 0o777,
                0o755
            );
        }
        assert_eq!(
            fs::read(registry_root.join("registry.json")).unwrap(),
            registry_bytes
        );
        assert_eq!(
            fs::read(envs_dir.join(format!("{task}.json"))).unwrap(),
            state_bytes
        );
        assert_eq!(before["errors"], json!([]));
        assert_eq!(before["processes"]["truncated"], false);
        assert_eq!(before["base_usage"]["truncated"], false);
        assert!(before["processes"]["records"]
            .as_array()
            .unwrap()
            .iter()
            .all(|record| record["fd_truncated"] == false));
        assert_eq!(before["roots"]["workspace"]["exists"], true);
        assert_eq!(before["roots"]["temp"]["exists"], true);
        assert_eq!(before["registry"]["registered"], true);
        assert_eq!(before["registry"]["entry"]["instance"], instance);
        let claimed = before["registry"]["entry"]["claimed_roots"]
            .as_array()
            .unwrap();
        assert!(claimed.iter().any(|root| root == &json!(workspace)));
        assert!(claimed.iter().any(|root| root == &json!(temporary)));
        assert_eq!(before["state"]["exists"], true);
        assert_eq!(before["cleanup"]["exists"], false);
        assert_eq!(before["registry"]["occupied_slots"], 1);
        assert_owned_queue(&before, &task);
        assert_unrelated_queue(&before);
        assert_eq!(before["queue_entries"]["mixed"]["deliveries"]["total"], 0);
        assert_eq!(before["queue_entries"]["unknown"]["tasks"]["total"], 0);
        assert_eq!(before["queue_entries"]["unknown"]["waits"]["total"], 0);
        assert_eq!(before["queue_entries"]["unknown"]["events"]["total"], 0);
        assert_eq!(before["queue_entries"]["unknown"]["deliveries"]["total"], 0);
        assert!(before["worktrees"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["path"]["path"] == json!(worktree)
                && item["git_worktree_list"]["entries"]
                    .as_array()
                    .is_some_and(|entries| entries.iter().any(|entry| entry
                        .as_str()
                        .is_some_and(|line| line.contains(worktree.to_str().unwrap()))))
                && item["git_worktree_list"]["total"]
                    .as_u64()
                    .is_some_and(|total| total >= 2)
                && item["git_worktree_list"]["truncated"] == false));
        assert_eq!(
            before["worktrees"][0]["git_worktree_list"]["truncated"],
            false
        );
        assert_eq!(before["worktrees"][0]["git_worktree_list"]["total"], 2);
        assert!(before["open_files"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["path"] == json!(open)));
        assert!(before["runtime_roots"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["exists"] == true));
        assert!(before["base_usage"]["files"].as_u64().unwrap() > 0);
        assert!(
            before["base_usage"]["allocated_blocks_512"]
                .as_u64()
                .unwrap()
                > 0
        );
        assert!(before["free_bytes_filesystem"].as_u64().is_some());
        let excluded_before = before["excluded"].clone();
        assert!(excluded_before
            .as_array()
            .is_some_and(|items| items.iter().all(|item| item["exists"] == true)));

        destroy_environment(&env).unwrap();
        let mut child = env.child.take().unwrap();
        let _ = child.kill();
        let _ = child.wait();
        assert!(!workspace.exists());
        assert!(!temporary.exists());
        assert!(!runtime.exists());
        assert!(!open.exists());
        assert!(!worktree.exists());

        let after = collect(&InventorySpec {
            base: &base,
            queue: &queue,
            registry_root: &registry_root,
            envs_dir: &envs_dir,
            plans_dir: &plans_dir,
            task_id: &task,
            instance_id: &instance,
            workspace_root: &workspace,
            temp_root: &temporary,
            runtime_roots: &runtime_roots,
            worktrees: &worktrees_before,
            open_files: &open_files_before,
            excluded: &excluded,
        });
        assert_eq!(fs::read(queue.join("queue.json")).unwrap(), queue_before);
        assert_eq!(after["errors"], json!([]));
        assert_eq!(after["roots"]["workspace"]["exists"], false);
        assert_eq!(after["roots"]["temp"]["exists"], false);
        assert_eq!(after["registry"]["registered"], false);
        assert_eq!(after["registry"]["entry"], J::Null);
        assert_eq!(after["state"]["exists"], false);
        assert_eq!(after["cleanup"]["exists"], false);
        assert_eq!(after["registry"]["occupied_slots"], 0);
        assert_eq!(after["processes"]["count"], 0);
        assert_eq!(after["processes"]["count_is_not_discovery_proof"], true);
        assert_eq!(after["base_usage"]["excluded_files"], 3);
        assert_eq!(after["base_usage"]["files"], 3);
        assert_eq!(after["base_usage"]["symlinks"], 1);
        assert_eq!(after["excluded"], excluded_before);
        assert!(after["runtime_roots"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["exists"] == false));
        assert!(after["open_files"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["exists"] == false));
        assert!(after["worktrees"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["path"]["exists"] == false
                && item["repo"]["exists"] == true
                && !item["git_worktree_list"]["entries"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|entry| entry
                        .as_str()
                        .is_some_and(|line| line.contains(worktree.to_str().unwrap())))));
        assert_eq!(
            after["worktrees"][0]["git_worktree_list"]["truncated"],
            false
        );
        assert_eq!(after["worktrees"][0]["git_worktree_list"]["total"], 1);
        assert!(
            outside.exists(),
            "collector must not delete outside observations"
        );
        assert!(outside_dir.join("secret").exists());
        assert_eq!(fs::read_to_string(&formal).unwrap(), "formal output");
        assert_eq!(fs::read_to_string(&user).unwrap(), "user asset");
        assert_eq!(fs::read_to_string(&selected).unwrap(), "{}");
        assert_owned_queue(&after, &task);
        assert_unrelated_queue(&after);
        assert_eq!(unrelated_values(&queue), unrelated_baseline);
        assert_eq!(after["queue_entries"]["mixed"]["deliveries"]["total"], 0);
        assert_eq!(after["queue_entries"]["unknown"]["tasks"]["total"], 0);
        assert_eq!(after["queue_entries"]["unknown"]["waits"]["total"], 0);
        assert_eq!(after["queue_entries"]["unknown"]["events"]["total"], 0);
        assert_eq!(after["queue_entries"]["unknown"]["deliveries"]["total"], 0);

        // Fixture reset only; not a production cleanup/lifecycle proof.
        reset_owned_queue(&queue, &task);
        assert_eq!(normalized_queue(&queue), queue_baseline);
        assert_eq!(unrelated_values(&queue), unrelated_baseline);
        let base_record = collect(&InventorySpec {
            base: &base,
            queue: &queue,
            registry_root: &registry_root,
            envs_dir: &envs_dir,
            plans_dir: &plans_dir,
            task_id: &task,
            instance_id: &instance,
            workspace_root: &workspace,
            temp_root: &temporary,
            runtime_roots: &runtime_roots,
            worktrees: &worktrees_before,
            open_files: &open_files_before,
            excluded: &excluded,
        });
        assert_eq!(base_record["registry"]["registered"], false);
        assert_eq!(base_record["queue_entries"]["owned"]["tasks"]["total"], 0);
        assert_eq!(base_record["queue_entries"]["owned"]["waits"]["total"], 0);
        assert_eq!(base_record["queue_entries"]["owned"]["events"]["total"], 0);
        assert_eq!(
            base_record["queue_entries"]["owned"]["deliveries"]["total"],
            0
        );
        assert_unrelated_queue(&base_record);
        if round == 1 {
            base_after_first = base_record["base_usage"].clone();
        } else {
            assert_eq!(
                base_record["base_usage"], base_after_first,
                "stable base grew across rounds"
            );
        }
    }
    assert!(outside.exists());
    let _ = fs::remove_file(outside);
}

#[test]
fn queue_inventory_reports_totals_truncation_and_malformed_sections() {
    let temp = temp();
    let base = temp.path().join("inventory");
    private_dir(&base);
    let queue = temp.path().join("queue");
    queue::init(&queue, None, 30).unwrap();
    let registry_root = temp.path().join("registry");
    let envs_dir = registry_root.join("envs");
    let plans_dir = registry_root.join("plans");
    let _registry_root = RegistryRootGuard::set(&registry_root);
    let mut state = queue::load_queue(&queue).unwrap();
    ensure_queue_maps(&mut state);
    for index in 0..130 {
        let key = format!("synthetic-owned-{index}");
        state["tasks"][key.as_str()] = json!({"lineage":{"requirement":"owned"}});
    }
    state["tasks"]["synthetic-unrelated"] = json!({"task":"synthetic-unrelated"});
    queue::save(&queue, &mut state).unwrap();
    let report = collect(&InventorySpec {
        base: &base,
        queue: &queue,
        registry_root: &registry_root,
        envs_dir: &envs_dir,
        plans_dir: &plans_dir,
        task_id: "owned",
        instance_id: "none",
        workspace_root: &base.join("missing-workspace"),
        temp_root: &base.join("missing-temp"),
        runtime_roots: &[],
        worktrees: &[],
        open_files: &[],
        excluded: &[],
    });
    assert_eq!(report["errors"], json!([]));
    let owned = &report["queue_entries"]["owned"]["tasks"];
    assert_eq!(owned["total"], 130);
    assert_eq!(owned["truncated"], true);
    assert_eq!(owned["keys"].as_array().unwrap().len(), 128);
    assert_page(
        &report["queue_entries"]["unrelated"],
        "tasks",
        1,
        &["synthetic-unrelated"],
        false,
    );

    let mut state = queue::load_queue(&queue).unwrap();
    state["tasks"] = json!("malformed");
    queue::save(&queue, &mut state).unwrap();
    let report = collect(&InventorySpec {
        base: &base,
        queue: &queue,
        registry_root: &registry_root,
        envs_dir: &envs_dir,
        plans_dir: &plans_dir,
        task_id: "owned",
        instance_id: "none",
        workspace_root: &base.join("missing-workspace"),
        temp_root: &base.join("missing-temp"),
        runtime_roots: &[],
        worktrees: &[],
        open_files: &[],
        excluded: &[],
    });
    assert!(report["errors"]
        .as_array()
        .unwrap()
        .iter()
        .any(|error| error
            .as_str()
            .is_some_and(|error| error.contains("queue section tasks"))));
}

#[test]
fn queue_inventory_reports_mixed_and_unknown_malformed_entries() {
    let temp = temp();
    let base = temp.path().join("inventory");
    private_dir(&base);
    let queue = temp.path().join("queue");
    queue::init(&queue, None, 30).unwrap();
    let registry_root = temp.path().join("registry");
    let envs_dir = registry_root.join("envs");
    let plans_dir = registry_root.join("plans");
    let _registry_root = RegistryRootGuard::set(&registry_root);
    let mut state = queue::load_queue(&queue).unwrap();
    ensure_queue_maps(&mut state);
    state["tasks"]["owned"] = json!({});
    state["tasks"]["unrelated"] = json!({});
    state["tasks"]["malformed"] = json!("bad");
    state["tasks"]["mismatch"] = json!({"task":"other"});
    state["waits"]["owned-wait"] = json!({"task":"owned"});
    state["waits"]["unrelated-wait"] = json!({"task":"unrelated"});
    state["waits"]["missing-wait"] = json!({"task":"missing"});
    state["waits"]["malformed-wait"] = json!("bad");
    state["waits"]["mismatched-wait"] = json!({"task":"mismatch"});
    state["events"]["owned-event"] = json!({"task":"owned"});
    state["events"]["unrelated-event"] = json!({"task":"unrelated"});
    state["events"]["missing-event"] = json!({"task":"missing"});
    state["events"]["malformed-event"] = json!("bad");
    state["deliveries"]["mixed"] = json!({"events":["owned-event","unrelated-event"]});
    state["deliveries"]["owned-only"] = json!({"events":["owned-event"]});
    state["deliveries"]["unrelated-only"] = json!({"events":["unrelated-event"]});
    state["deliveries"]["missing-event"] = json!({"events":["ghost-event"]});
    state["deliveries"]["empty"] = json!({"events":[]});
    state["deliveries"]["malformed"] = json!({"events":"bad"});
    state["deliveries"]["nonstring"] = json!({"events":[1]});
    queue::save(&queue, &mut state).unwrap();

    let report = collect(&InventorySpec {
        base: &base,
        queue: &queue,
        registry_root: &registry_root,
        envs_dir: &envs_dir,
        plans_dir: &plans_dir,
        task_id: "owned",
        instance_id: "none",
        workspace_root: &base.join("missing-workspace"),
        temp_root: &base.join("missing-temp"),
        runtime_roots: &[],
        worktrees: &[],
        open_files: &[],
        excluded: &[],
    });
    assert!(!report["errors"].as_array().unwrap().is_empty());
    assert_page(
        &report["queue_entries"]["owned"],
        "tasks",
        1,
        &["owned"],
        false,
    );
    assert_page(
        &report["queue_entries"]["unrelated"],
        "tasks",
        1,
        &["unrelated"],
        false,
    );
    assert_page(
        &report["queue_entries"]["unknown"],
        "tasks",
        2,
        &["malformed", "mismatch"],
        false,
    );
    assert_page(
        &report["queue_entries"]["unknown"],
        "waits",
        3,
        &["malformed-wait", "mismatched-wait", "missing-wait"],
        false,
    );
    assert_page(
        &report["queue_entries"]["unknown"],
        "events",
        2,
        &["malformed-event", "missing-event"],
        false,
    );
    assert_page(
        &report["queue_entries"]["owned"],
        "deliveries",
        1,
        &["owned-only"],
        false,
    );
    assert_page(
        &report["queue_entries"]["unrelated"],
        "deliveries",
        1,
        &["unrelated-only"],
        false,
    );
    let mixed = &report["queue_entries"]["mixed"]["deliveries"];
    assert_eq!(mixed["total"], 1);
    assert_eq!(mixed["truncated"], false);
    let entry = &mixed["entries"][0];
    assert_eq!(entry["id"], "mixed");
    assert_eq!(entry["owned_events"], json!(["owned-event"]));
    assert_eq!(entry["unrelated_events"], json!(["unrelated-event"]));
    assert_page(
        &report["queue_entries"]["unknown"],
        "deliveries",
        4,
        &["empty", "malformed", "missing-event", "nonstring"],
        false,
    );
}

#[test]
fn collector_does_not_create_missing_registry_base_or_queue() {
    let temp = temp();
    let campaign = temp.path().join("campaign");
    private_dir(&campaign);
    let sentinel = campaign.join("sentinel");
    fs::write(&sentinel, "unchanged").unwrap();
    let base = campaign.join("missing-base");
    let queue = campaign.join("missing-queue");
    let registry_root = campaign.join("missing-registry");
    let envs_dir = registry_root.join("envs");
    let plans_dir = registry_root.join("plans");
    let before = fs::metadata(&campaign).unwrap();
    let before_mode = before.permissions().mode();
    let before_entries = fs::read_dir(&campaign).unwrap().count();
    let report = collect(&InventorySpec {
        base: &base,
        queue: &queue,
        registry_root: &registry_root,
        envs_dir: &envs_dir,
        plans_dir: &plans_dir,
        task_id: "owned",
        instance_id: "none",
        workspace_root: &campaign.join("missing-workspace"),
        temp_root: &campaign.join("missing-temp"),
        runtime_roots: &[],
        worktrees: &[],
        open_files: &[],
        excluded: &[],
    });
    assert!(!base.exists());
    assert!(!queue.exists());
    assert!(!registry_root.exists());
    assert!(!envs_dir.exists());
    assert!(!plans_dir.exists());
    assert_eq!(
        fs::metadata(&campaign).unwrap().permissions().mode(),
        before_mode
    );
    assert_eq!(fs::read_dir(&campaign).unwrap().count(), before_entries);
    assert_eq!(fs::read_to_string(&sentinel).unwrap(), "unchanged");
    assert!(report["errors"]
        .as_array()
        .unwrap()
        .iter()
        .any(|error| error
            .as_str()
            .is_some_and(|error| error.contains("declared base"))));
    assert!(report["errors"]
        .as_array()
        .unwrap()
        .iter()
        .any(|error| error
            .as_str()
            .is_some_and(|error| error.contains("queue read"))));
}
