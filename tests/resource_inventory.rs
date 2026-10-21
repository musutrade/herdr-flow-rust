//! Synthetic stable-base validation for the test-only resource inventory.
#[path = "common/resource_inventory.rs"]
mod resource_inventory;

use herdr_flow::{queue, task_env::*, task_runtime};
use resource_inventory::{collect, InventorySpec};
use serde_json::{json, Value as J};
use std::{
    fs,
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Mutex,
    time::{Duration, Instant},
};
use tempfile::TempDir;

static LOCK: Mutex<()> = Mutex::new(());

fn temp() -> TempDir {
    tempfile::Builder::new()
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir()
        .unwrap()
}

fn private_dir(path: &Path) {
    fs::DirBuilder::new().mode(0o700).create_all(path).unwrap();
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

fn create_worktree(env: &mut TaskEnvironment, round: usize) -> PathBuf {
    let repo = env.workspace_root.join("repo");
    fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    git(
        &repo,
        &["config", "user.email", "inventory@example.invalid"],
    );
    git(&repo, &["config", "user.name", "Inventory fixture"]);
    fs::write(repo.join("file"), "base").unwrap();
    git(&repo, &["add", "file"]);
    git(&repo, &["commit", "-qm", "base"]);
    let worktree = env.workspace_root.join(format!("linked-{round}"));
    let branch = format!("round-{round}");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            &branch,
            worktree.to_str().unwrap(),
        ],
    );
    env.register_worktree(&worktree).unwrap();
    worktree
}

fn add_queue_round(q: &Path, task: &str) {
    let mut state = queue::load_queue(q).unwrap();
    for section in ["tasks", "waits", "events", "deliveries"] {
        if !state[section].is_object() {
            state[section] = json!({});
        }
    }
    state["tasks"][task] = json!({"task":task});
    state["tasks"]["unrelated"] = json!({"task":"unrelated","keep":true});
    let wait = format!("{task}-wait");
    let event = format!("{task}-event");
    let delivery = format!("{task}-delivery");
    state["waits"][wait.as_str()] = json!({"id":wait,"task":task});
    state["waits"]["unrelated-wait"] = json!({"id":"unrelated-wait","task":"unrelated"});
    state["events"][event.as_str()] = json!({"id":event,"task":task});
    state["events"]["unrelated-event"] = json!({"id":"unrelated-event","task":"unrelated"});
    state["deliveries"][delivery.as_str()] = json!({"id":delivery,"events":[event]});
    state["deliveries"]["unrelated-delivery"] =
        json!({"id":"unrelated-delivery","events":["unrelated-event"]});
    queue::save(q, &mut state).unwrap();
}

fn remove_owned_queue(q: &Path, task: &str) {
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

fn find_fd(report: &J, outside: &Path) -> Option<(i32, bool)> {
    report["processes"]["records"]
        .as_array()?
        .iter()
        .find_map(|record| {
            let pid = record["pid"].as_i64()? as i32;
            let found = record["fds"].as_array()?.iter().any(|fd| {
                fd["target"]
                    .as_str()
                    .is_some_and(|target| Path::new(target) == outside)
            });
            found.then_some((pid, record["complete"] == true))
        })
}

fn assert_unrelated_queue(report: &J) {
    assert_eq!(
        report["queue_entries"]["unrelated"]["tasks"],
        json!(["unrelated"])
    );
    assert_eq!(
        report["queue_entries"]["unrelated"]["waits"],
        json!(["unrelated-wait"])
    );
    assert_eq!(
        report["queue_entries"]["unrelated"]["events"],
        json!(["unrelated-event"])
    );
    assert_eq!(
        report["queue_entries"]["unrelated"]["deliveries"],
        json!(["unrelated-delivery"])
    );
}

#[test]
fn stable_base_inventory_collects_and_preserves_ownership_across_two_rounds() {
    let _lock = LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let temp = temp();
    let base = temp.path().join("campaign");
    private_dir(&base);
    let queue = temp.path().join("queue");
    queue::init(&queue, None, 30).unwrap();
    let formal = base.join("formal.txt");
    let user = base.join("user.txt");
    let selected = base.join("selected-config.json");
    let outside = temp.path().join("outside-fd.txt");
    fs::write(&formal, "formal output").unwrap();
    fs::write(&user, "user asset").unwrap();
    fs::write(&selected, "{}").unwrap();
    fs::write(&outside, "outside open fd").unwrap();
    std::env::set_var("HERDR_TASK_ENV_ROOT", base.join("registry"));
    let excluded = vec![formal.clone(), user.clone(), selected.clone()];
    let mut base_after_first = J::Null;

    for round in 1..=2 {
        let task = format!("inventory-{round}");
        let round_root = base.join(format!("round-{round}"));
        private_dir(&round_root);
        let workspace = round_root.join("workspace");
        let temporary = round_root.join("temporary");
        let mut env = create_environment(
            &task,
            &workspace,
            vec![DeliveryTarget::new(formal.clone(), "file".into())],
            &temporary,
        )
        .unwrap();
        let instance = env.instance_id.clone();
        let runtime = task_runtime::prepare(&env, "inventory-run").unwrap();
        fs::write(runtime.join("cache/marker"), "cache").unwrap();
        let open = env.workspace_root.join("open.txt");
        let handle = fs::File::create(&open).unwrap();
        env.register_handle(&open, handle).unwrap();
        let worktree = create_worktree(&mut env, round);
        add_queue_round(&queue, &task);
        let queue_before = fs::read(queue.join("queue.json")).unwrap();
        let mut child = env
            .command("/bin/sh")
            .args(["-c", "exec 9<\"$1\"; sleep 30", "sh"])
            .arg(&outside)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        env.register_process(child.id() as i32).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        let before = loop {
            let runtime_roots = vec![runtime.clone()];
            let report = collect(&InventorySpec {
                base: &base,
                queue: &queue,
                task_id: &task,
                instance_id: &instance,
                workspace_root: &workspace,
                temp_root: &temporary,
                runtime_roots: &runtime_roots,
                worktrees: &env.worktree_registrations,
                open_files: &env.open_files,
                excluded: &excluded,
            });
            if let Some((_, complete)) = find_fd(&report, &outside) {
                assert!(complete, "pid/start changed during fd sampling");
                break report;
            }
            assert!(Instant::now() < deadline, "outside fd was not sampled");
            std::thread::sleep(Duration::from_millis(50));
        };
        assert_eq!(fs::read(queue.join("queue.json")).unwrap(), queue_before);
        assert_eq!(before["registry"]["registered"], true);
        assert_eq!(before["state"]["exists"], true);
        assert_eq!(before["cleanup"]["exists"], false);
        assert_eq!(before["registry"]["occupied_slots"], 1);
        assert_eq!(before["queue_entries"]["owned"]["tasks"], json!([task]));
        assert_eq!(
            before["queue_entries"]["owned"]["waits"],
            json!([format!("{task}-wait")])
        );
        assert_eq!(
            before["queue_entries"]["owned"]["events"],
            json!([format!("{task}-event")])
        );
        assert_eq!(
            before["queue_entries"]["owned"]["deliveries"],
            json!([format!("{task}-delivery")])
        );
        assert_unrelated_queue(&before);
        assert!(before["worktrees"]
            .as_array()
            .is_some_and(|items| !items.is_empty()));
        assert!(before["open_files"]
            .as_array()
            .is_some_and(|items| !items.is_empty()));
        assert!(before["runtime_roots"]
            .as_array()
            .is_some_and(|items| items.iter().all(|item| item["exists"] == true)));
        assert!(before["base_usage"]["files"].as_u64().unwrap() > 0);
        assert!(
            before["base_usage"]["allocated_blocks_512"]
                .as_u64()
                .unwrap()
                > 0
        );
        assert!(before["free_bytes_filesystem"].as_u64().is_some());
        assert!(before["excluded"]
            .as_array()
            .is_some_and(|items| items.iter().all(|item| item["exists"] == true)));

        destroy_environment(&env).unwrap();
        let _ = child.kill();
        let _ = child.wait();
        assert!(!workspace.exists());
        assert!(!temporary.exists());
        assert!(!runtime.exists());
        assert!(!open.exists());
        assert!(!worktree.exists());
        fs::remove_dir_all(&round_root).unwrap();

        let after_spec = InventorySpec {
            base: &base,
            queue: &queue,
            task_id: &task,
            instance_id: &instance,
            workspace_root: &workspace,
            temp_root: &temporary,
            runtime_roots: &[],
            worktrees: &[],
            open_files: &[],
            excluded: &excluded,
        };
        let after = collect(&after_spec);
        assert_eq!(fs::read(queue.join("queue.json")).unwrap(), queue_before);
        assert_eq!(after["registry"]["registered"], false);
        assert_eq!(after["state"]["exists"], false);
        assert_eq!(after["cleanup"]["exists"], false);
        assert_eq!(after["registry"]["occupied_slots"], 0);
        assert_eq!(after["processes"]["count"], 0);
        assert_eq!(after["worktrees"], json!([]));
        assert_eq!(after["open_files"], json!([]));
        assert!(after["runtime_roots"].as_array().is_some_and(Vec::is_empty));
        assert!(after["base_usage"]["files"].as_u64().unwrap() > 0);
        assert_eq!(after["base_usage"]["excluded_files"], 3);
        assert!(after["excluded"]
            .as_array()
            .is_some_and(|items| items.iter().all(|item| item["exists"] == true)));
        assert!(
            outside.exists(),
            "collector must not delete outside observations"
        );
        assert_eq!(fs::read_to_string(&formal).unwrap(), "formal output");
        assert_eq!(fs::read_to_string(&user).unwrap(), "user asset");
        assert_eq!(fs::read_to_string(&selected).unwrap(), "{}");
        assert_eq!(after["queue_entries"]["owned"]["tasks"], json!([task]));
        assert_eq!(
            after["queue_entries"]["owned"]["waits"],
            json!([format!("{task}-wait")])
        );
        assert_eq!(
            after["queue_entries"]["owned"]["events"],
            json!([format!("{task}-event")])
        );
        assert_eq!(
            after["queue_entries"]["owned"]["deliveries"],
            json!([format!("{task}-delivery")])
        );
        assert_unrelated_queue(&after);

        remove_owned_queue(&queue, &task);
        let cleaned = queue::load_queue(&queue).unwrap();
        assert!(cleaned["tasks"].get(task.as_str()).is_none());
        assert!(cleaned["tasks"]["unrelated"]["keep"] == true);
        let base_record = collect(&InventorySpec {
            base: &base,
            queue: &queue,
            task_id: &task,
            instance_id: &instance,
            workspace_root: &workspace,
            temp_root: &temporary,
            runtime_roots: &[],
            worktrees: &[],
            open_files: &[],
            excluded: &excluded,
        });
        assert_eq!(base_record["registry"]["registered"], false);
        assert_eq!(
            base_record["queue_entries"]["unrelated"]["tasks"],
            json!(["unrelated"])
        );
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
