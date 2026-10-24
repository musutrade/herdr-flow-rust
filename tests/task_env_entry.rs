//! No-model fixtures for the preparatory CLI and non-mutating status.
use herdr_flow::task_env::{self, TaskEnvironment};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Mutex, MutexGuard};

static LOCK: Mutex<()> = Mutex::new(());
struct Fixture {
    dir: tempfile::TempDir,
    _lock: MutexGuard<'static, ()>,
}
impl Fixture {
    fn new() -> Self {
        let lock = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::Builder::new()
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        std::env::set_var("HERDR_TASK_ENV_ROOT", dir.path().join("control"));
        std::env::remove_var("HERDR_TASK_ENV_LIMIT");
        Self { dir, _lock: lock }
    }
    fn path(&self, path: &str) -> PathBuf {
        self.dir.path().join(path)
    }
    fn create(&self) -> TaskEnvironment {
        task_env::create_environment("one", &self.path("work"), vec![], &self.path("temp")).unwrap()
    }
    fn spec(&self) -> Value {
        json!({"workspace_root":self.path("work"),"temp_root":self.path("temp"),"delivery_targets":[]})
    }
    fn cli(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_herdr-flow"))
            .arg("task-env")
            .args(args)
            .env("HERDR_TASK_ENV_ROOT", self.path("control"))
            .output()
            .unwrap()
    }
    fn create_cli(&self, spec: &Value) -> Output {
        write(&self.path("spec.json"), spec);
        self.cli(&[
            "create",
            "--task",
            "one",
            "--spec",
            self.path("spec.json").to_str().unwrap(),
        ])
    }
    fn status(&self) -> Value {
        task_env::status("one").unwrap()
    }
    fn plan(&self, env: &TaskEnvironment) {
        fs::create_dir_all(self.path("control/plans")).unwrap();
        fs::set_permissions(
            self.path("control/plans"),
            fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        write(
            &self.path("control/plans/one.json"),
            &json!({
                "task_id":"one","instance_id":env.instance_id,"delivery_confirmed":false,
                "roots":[{"path":env.workspace_root,"identity":env.workspace_identity},
                    {"path":env.temp_root,"identity":env.temp_identity}],
                "pending_processes":[],"pending_worktrees":[],"pending_files":[],
                "pending_root_removals":[env.workspace_root,env.temp_root],
                "quarantine":null,"started_at":0.0,"updated_at":0.0,"attempts":0,"last_error":null
            }),
        );
    }
}
fn write(path: &Path, value: &Value) {
    fs::write(path, serde_json::to_vec(value).unwrap()).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}
fn output(o: Output, code: i32) -> Value {
    assert_eq!(
        o.status.code(),
        Some(code),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
    serde_json::from_slice(&o.stdout).unwrap()
}
fn snapshot(root: &Path) -> BTreeMap<PathBuf, Value> {
    let mut result = BTreeMap::new();
    if !root.exists() {
        return result;
    }
    let m = fs::symlink_metadata(root).unwrap();
    result.insert(
        root.to_path_buf(),
        json!({"mode":m.mode(),"ino":m.ino(),
        "mtime":m.mtime(),"nsec":m.mtime_nsec(),
        "content":if m.is_file(){Some(fs::read(root).unwrap())}else{None}}),
    );
    if m.is_dir() {
        for entry in fs::read_dir(root).unwrap() {
            result.extend(snapshot(&entry.unwrap().path()));
        }
    }
    result
}

#[test]
fn absent_status_creates_no_control_directories_or_locks() {
    let f = Fixture::new();
    let before = snapshot(f.dir.path());
    let v = output(f.cli(&["status", "--task", "one"]), 0);
    assert_eq!(v["state"], "absent");
    assert_eq!(v["occupied_slots"], 0);
    assert_eq!(v["recovery"], "no_op");
    assert_eq!(snapshot(f.dir.path()), before);
    assert!(!f.path("control").exists());
}

#[test]
fn status_while_owned_is_readonly_and_does_not_take_lock() {
    let f = Fixture::new();
    let env = f.create();
    let before = snapshot(f.dir.path());
    let v = output(f.cli(&["status", "--task", "one"]), 0);
    assert_eq!(v["state"], "active");
    assert_eq!(v["registered"], true);
    assert_eq!(v["instance_id"], env.instance_id);
    assert_eq!(v["occupied_slots"], 1);
    assert_eq!(v["interrupted"], false);
    assert_eq!(v["cleanup_authorized"], false);
    assert_eq!(v["ownership_checked"], false);
    assert_eq!(snapshot(f.dir.path()), before);
    assert!(!f.cli(&["recover", "--task", "one"]).status.success());
}

#[test]
fn create_cli_persists_once_and_active_no_plan_recovery_is_noop() {
    let f = Fixture::new();
    let v = output(f.create_cli(&f.spec()), 0);
    assert_eq!(v["state"], "active");
    assert_eq!(v["task_id"], "one");
    let before = snapshot(&f.path("control"));
    assert!(!f.create_cli(&f.spec()).status.success());
    assert_eq!(snapshot(&f.path("control")), before);
    let v = output(f.cli(&["recover", "--task", "one"]), 0);
    assert_eq!(v["recovered"], false);
    assert!(f.path("work").exists());
    assert_eq!(snapshot(&f.path("control")), before);
}

#[test]
fn active_plus_plan_counts_twice_and_explicit_recover_destroys_fixture() {
    let f = Fixture::new();
    let env = f.create();
    f.plan(&env);
    let before = snapshot(&f.path("control"));
    let v = f.status();
    assert_eq!(v["state"], "active");
    assert_eq!(v["occupied_slots"], 2);
    assert_eq!(v["interrupted"], true);
    assert_eq!(v["recovery"], "would_destroy");
    assert_eq!(snapshot(&f.path("control")), before);
    drop(env);
    let v = output(f.cli(&["recover", "--task", "one"]), 0);
    assert_eq!(v["recovered"], true);
    assert!(!f.path("work").exists());
    assert!(!f.path("temp").exists());
    assert_eq!(f.status()["state"], "absent");
}

#[test]
fn plan_only_and_destroy_failed_are_observed_without_restoring() {
    let f = Fixture::new();
    let mut env = f.create();
    env.mark_destroy_failed().unwrap();
    f.plan(&env);
    assert_eq!(f.status()["state"], "destroy_failed");
    assert_eq!(f.status()["occupied_slots"], 2);
    fs::remove_file(&env.state_file).unwrap();
    assert_eq!(f.status()["state"], "plan_only");
    assert_eq!(f.status()["registered"], true);
    assert_eq!(f.status()["occupied_slots"], 1);
    drop(env);
    assert_eq!(
        output(f.cli(&["recover", "--task", "one"]), 0)["recovered"],
        true
    );
}

#[test]
fn creating_recovery_and_missing_recovery_keep_existing_library_semantics() {
    let f = Fixture::new();
    let env = f.create();
    let mut doc = serde_json::to_value(&env).unwrap();
    doc["state"] = json!("creating");
    write(&env.state_file, &doc);
    drop(env);
    assert_eq!(f.status()["state"], "creating");
    assert_eq!(f.status()["recovery"], "would_destroy");
    assert_eq!(
        output(f.cli(&["recover", "--task", "one"]), 0)["recovered"],
        true
    );
    assert_eq!(
        output(f.cli(&["recover", "--task", "absent"]), 0)["recovered"],
        false
    );
    assert!(f.path("control/envs").is_dir());
}

#[test]
fn cli_rejects_unknown_duplicate_missing_and_wrong_action_options_before_state() {
    let f = Fixture::new();
    for args in [
        vec![],
        vec!["create"],
        vec!["status", "--task"],
        vec!["status", "--task", "one", "--task", "two"],
        vec!["status", "--task", "one", "--unknown", "x"],
        vec!["status", "--task", "one", "--spec", "x"],
        vec!["recover", "--task", "one", "--spec", "x"],
        vec!["destroy", "--task", "one"],
        vec!["create", "--task", "one"],
        vec!["status", "--task", "../escape"],
    ] {
        assert!(!f.cli(&args).status.success(), "{args:?}");
    }
    assert!(f.cli(&["--help"]).status.success());
    assert!(!f.path("control").exists());
}

#[test]
fn spec_rejects_unknown_missing_and_relative_paths_before_creation() {
    let f = Fixture::new();
    for key in ["workspace_root", "temp_root", "delivery_targets"] {
        let mut spec = f.spec();
        spec.as_object_mut().unwrap().remove(key);
        assert!(!f.create_cli(&spec).status.success());
    }
    let mut spec = f.spec();
    spec["task_id"] = json!("other");
    assert!(!f.create_cli(&spec).status.success());
    for key in ["workspace_root", "temp_root"] {
        let mut spec = f.spec();
        spec[key] = json!("relative");
        assert!(!f.create_cli(&spec).status.success());
    }
    for target in [
        json!({"path":"relative","target_type":"file"}),
        json!({"path":f.path("out"),"target_type":"file","extra":true}),
    ] {
        let mut spec = f.spec();
        spec["delivery_targets"] = json!([target]);
        assert!(!f.create_cli(&spec).status.success());
    }
    assert!(!f.path("control").exists());
}

#[test]
fn create_keeps_vacancy_overlap_capacity_and_live_owner_guards() {
    let f = Fixture::new();
    let mut spec = f.spec();
    spec["temp_root"] = spec["workspace_root"].clone();
    assert!(!f.create_cli(&spec).status.success());
    fs::create_dir(f.path("work")).unwrap();
    fs::write(f.path("work/user.txt"), "keep").unwrap();
    assert!(!f.create_cli(&f.spec()).status.success());
    assert_eq!(fs::read_to_string(f.path("work/user.txt")).unwrap(), "keep");
    fs::remove_file(f.path("work/user.txt")).unwrap();
    let env = f.create();
    assert!(!f.create_cli(&f.spec()).status.success());
    std::env::set_var("HERDR_TASK_ENV_LIMIT", "1");
    let other = json!({"workspace_root":f.path("other-work"),"temp_root":f.path("other-temp"),"delivery_targets":[]});
    write(&f.path("other.json"), &other);
    assert!(!f
        .cli(&[
            "create",
            "--task",
            "two",
            "--spec",
            f.path("other.json").to_str().unwrap()
        ])
        .status
        .success());
    assert!(!f.path("other-work").exists());
    drop(env);
}

#[test]
fn malformed_symlink_and_oversized_documents_report_unknown_without_mutation() {
    let f = Fixture::new();
    let env = f.create();
    let original = fs::read(&env.state_file).unwrap();
    for content in [
        b"broken".to_vec(),
        vec![b' '; 1024 * 1024 + 1],
        b"{}".to_vec(),
    ] {
        fs::write(&env.state_file, content).unwrap();
        let before = snapshot(&f.path("control"));
        let v = output(f.cli(&["status", "--task", "one"]), 1);
        assert_eq!(v["state"], "unknown");
        assert_eq!(v["occupied_slots"], Value::Null);
        assert_eq!(snapshot(&f.path("control")), before);
    }
    fs::write(&env.state_file, original).unwrap();
    let foreign = f.path("foreign.json");
    fs::write(&foreign, "{}").unwrap();
    symlink(&foreign, f.path("control/envs/two.json")).unwrap();
    assert_eq!(f.status()["state"], "unknown");
    assert_eq!(fs::read_to_string(foreign).unwrap(), "{}");
}

#[test]
fn registry_instance_and_filename_inconsistencies_are_unknown() {
    let f = Fixture::new();
    let env = f.create();
    f.plan(&env);
    let plan = f.path("control/plans/one.json");
    let mut value: Value = serde_json::from_slice(&fs::read(&plan).unwrap()).unwrap();
    value["instance_id"] = json!("foreign");
    write(&plan, &value);
    assert_eq!(f.status()["state"], "unknown");
    fs::remove_file(plan).unwrap();
    fs::rename(&env.state_file, f.path("control/envs/two.json")).unwrap();
    assert_eq!(f.status()["state"], "unknown");
}

#[test]
fn symlink_control_directory_even_with_missing_leaf_is_unknown() {
    let f = Fixture::new();
    symlink(f.path("missing"), f.path("control")).unwrap();
    assert_eq!(f.status()["state"], "unknown");
    assert!(!f.path("missing").exists());
}

#[test]
fn status_rejects_writable_directory_without_chmod() {
    let f = Fixture::new();
    let _env = f.create();
    fs::set_permissions(f.path("control/envs"), fs::Permissions::from_mode(0o775)).unwrap();
    let before = snapshot(&f.path("control"));
    assert_eq!(f.status()["state"], "unknown");
    assert_eq!(snapshot(&f.path("control")), before);
}

#[test]
fn concurrent_cli_create_has_one_winner_and_keeps_external_delivery_target() {
    let f = Fixture::new();
    let mut spec = f.spec();
    spec["delivery_targets"] = json!([{"path":f.path("delivery"),"target_type":"directory"}]);
    fs::create_dir(f.path("delivery")).unwrap();
    fs::write(f.path("delivery/user"), "external").unwrap();
    write(&f.path("spec.json"), &spec);
    let launch = || {
        Command::new(env!("CARGO_BIN_EXE_herdr-flow"))
            .args(["task-env", "create", "--task", "one", "--spec"])
            .arg(f.path("spec.json"))
            .env("HERDR_TASK_ENV_ROOT", f.path("control"))
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap()
    };
    let a = launch();
    let b = launch();
    let a = a.wait_with_output().unwrap();
    let b = b.wait_with_output().unwrap();
    assert_eq!(
        usize::from(a.status.success()) + usize::from(b.status.success()),
        1
    );
    assert_eq!(f.status()["occupied_slots"], 1);
    assert_eq!(
        fs::read_to_string(f.path("delivery/user")).unwrap(),
        "external"
    );
}

#[test]
fn malformed_registry_and_foreign_document_never_look_absent() {
    let f = Fixture::new();
    let env = f.create();
    let path = f.path("control/registry.json");
    let original: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    for bad in [
        json!({}),
        json!({"entries":[]}),
        json!({"entries":{"bad":{}}}),
    ] {
        write(&path, &bad);
        assert_eq!(f.status()["state"], "unknown");
    }
    let mut wrong = original.clone();
    wrong["entries"]["one"]["instance_id"] = json!("foreign");
    write(&path, &wrong);
    assert_eq!(f.status()["state"], "unknown");
    wrong = original.clone();
    wrong["entries"]["other"] = wrong["entries"]["one"].clone();
    write(&path, &wrong);
    assert_eq!(f.status()["state"], "unknown");
    write(&path, &original);
    write(
        &f.path("control/envs/foreign.json"),
        &json!({"task_id":"foreign"}),
    );
    assert_eq!(f.status()["state"], "unknown");
    fs::remove_file(f.path("control/envs/foreign.json")).unwrap();
    fs::remove_file(path).unwrap();
    assert_eq!(f.status()["state"], "unknown");
    drop(env);
}

#[test]
fn status_scan_limit_and_non_regular_entries_are_explicit_errors() {
    let f = Fixture::new();
    let _env = f.create();
    fs::create_dir(f.path("control/envs/invalid.json")).unwrap();
    assert_eq!(f.status()["state"], "unknown");
    fs::remove_dir(f.path("control/envs/invalid.json")).unwrap();
    for n in 0..1024 {
        fs::write(f.path(&format!("control/envs/non-json-{n}")), "").unwrap();
    }
    let before = snapshot(&f.path("control"));
    let v = f.status();
    assert_eq!(v["state"], "unknown");
    assert!(v["errors"].to_string().contains("1024"));
    assert_eq!(snapshot(&f.path("control")), before);
}

#[test]
fn default_home_and_relative_control_override_are_pure_for_absent_status() {
    let f = Fixture::new();
    let default = Command::new(env!("CARGO_BIN_EXE_herdr-flow"))
        .args(["task-env", "status", "--task", "one"])
        .env_remove("HERDR_TASK_ENV_ROOT")
        .env("HOME", f.dir.path())
        .output()
        .unwrap();
    assert_eq!(
        output(default, 0)["control_root"],
        f.path(".herdr/flow/task_environments").to_str().unwrap()
    );
    assert!(!f.path(".herdr").exists());
    let relative = Command::new(env!("CARGO_BIN_EXE_herdr-flow"))
        .args(["task-env", "status", "--task", "one"])
        .env("HERDR_TASK_ENV_ROOT", "relative")
        .current_dir(f.dir.path())
        .output()
        .unwrap();
    assert_eq!(
        output(relative, 0)["control_root"],
        f.path("relative").to_str().unwrap()
    );
    assert!(!f.path("relative").exists());
    assert_eq!(
        output(f.cli(&["recover", "--task", "missing"]), 0)["recovered"],
        false
    );
    assert!(f.path("control/envs").is_dir());
}

#[test]
fn total_snapshot_budget_reports_unknown_instead_of_partial_accounting() {
    let f = Fixture::new();
    let env = f.create();
    let mut doc = serde_json::to_value(&env).unwrap();
    doc["state"] = json!("creating");
    doc["padding"] = json!("x".repeat(950_000));
    for n in 0..9 {
        let name = format!("extra-{n}");
        doc["task_id"] = json!(name);
        let path = f.path(&format!("control/envs/{name}.json"));
        doc["state_file"] = json!(path);
        write(&path, &doc);
    }
    let v = f.status();
    assert_eq!(v["state"], "unknown");
    assert_eq!(v["occupied_slots"], Value::Null);
    assert!(v["errors"].to_string().contains("8 MiB"));
}

#[test]
fn parent_components_in_control_path_match_canonical_stored_identity() {
    let f = Fixture::new();
    fs::create_dir(f.path("prefix")).unwrap();
    fs::set_permissions(f.path("prefix"), fs::Permissions::from_mode(0o700)).unwrap();
    std::env::set_var("HERDR_TASK_ENV_ROOT", f.path("prefix/../control"));
    let env = f.create();
    let before = snapshot(&f.path("control"));
    assert_eq!(f.status()["state"], "active");
    assert_eq!(snapshot(&f.path("control")), before);
    assert_eq!(env.state_file, f.path("control/envs/one.json"));
}

#[test]
fn inconsistent_root_layout_and_plan_obligations_are_unknown() {
    let f = Fixture::new();
    let env = f.create();
    f.plan(&env);
    let ep = f.path("control/envs/one.json");
    let pp = f.path("control/plans/one.json");
    let original: Value = serde_json::from_slice(&fs::read(&ep).unwrap()).unwrap();
    let plan: Value = serde_json::from_slice(&fs::read(&pp).unwrap()).unwrap();
    for (key, bad) in [
        ("workspace_root", json!(f.path("control/owned"))),
        ("temp_root", json!(f.path("work/nested"))),
        (
            "delivery_targets",
            json!([{"path":f.path("work/out"),"target_type":"file"}]),
        ),
    ] {
        let mut doc = original.clone();
        doc[key] = bad;
        write(&ep, &doc);
        let v = f.status();
        assert_eq!(v["state"], "unknown");
        assert_eq!(v["snapshot"], false);
    }
    write(&ep, &original);
    for bad in [json!(f.path("foreign")), json!(f.path("control/envs"))] {
        let mut doc = plan.clone();
        doc["roots"][0]["path"] = bad;
        doc["pending_root_removals"] = json!([]);
        write(&pp, &doc);
        assert_eq!(f.status()["state"], "unknown");
    }
    let mut doc = plan.clone();
    doc["pending_root_removals"] = json!([f.path("foreign")]);
    write(&pp, &doc);
    assert_eq!(f.status()["state"], "unknown");
    // Plan-only still checks registry roots, and without registry rejects control overlap.
    fs::remove_file(ep).unwrap();
    doc = plan.clone();
    doc["roots"][0]["path"] = json!(f.path("foreign"));
    doc["pending_root_removals"] = json!([]);
    write(&pp, &doc);
    assert_eq!(f.status()["state"], "unknown");
    fs::remove_file(f.path("control/registry.json")).unwrap();
    doc["roots"][0]["path"] = json!(f.path("control"));
    write(&pp, &doc);
    assert_eq!(f.status()["state"], "unknown");
}

#[test]
fn empty_target_type_is_rejected_before_create() {
    let f = Fixture::new();
    for kind in ["", " \t"] {
        let mut spec = f.spec();
        spec["delivery_targets"] = json!([{"path":f.path("out"),"target_type":kind}]);
        assert!(!f.create_cli(&spec).status.success());
    }
    assert!(!f.path("control").exists());
}
