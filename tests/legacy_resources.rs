//! Legacy queue migration with an already-owned environment; no arbitrary adoption.
use herdr_flow::{progress::lineage, queue, task_env::*, util};
use serde_json::json;
use std::{
    ffi::OsString,
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt},
    sync::{Mutex, MutexGuard},
};

static ENV_LOCK: Mutex<()> = Mutex::new(());
struct RegistryRootGuard {
    previous: Option<OsString>,
    _lock: MutexGuard<'static, ()>,
}
impl RegistryRootGuard {
    fn set(path: &std::path::Path) -> Self {
        let lock = ENV_LOCK.lock().unwrap();
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

#[test]
fn queue_migration_leaves_resources_unchanged_then_environment_restores_separately() {
    let temp = tempfile::Builder::new()
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir()
        .unwrap();
    let _registry_root = RegistryRootGuard::set(&temp.path().join("registry"));
    let user_asset = temp.path().join("user-owned.txt");
    let formal = temp.path().join("formal.txt");
    fs::write(&user_asset, "unrelated user asset").unwrap();
    let env = create_environment(
        "legacy",
        &temp.path().join("workspace"),
        vec![DeliveryTarget::new(formal.clone(), "file".into())],
        &temp.path().join("temporary"),
    )
    .unwrap();
    let instance = env.instance_id.clone();
    let artifact = env.workspace_root.join("undelivered.txt");
    fs::write(&artifact, "existing undelivered result").unwrap();
    let q = env.temp_root.join("queue");
    let mut state = queue::init(&q, None, 30).unwrap();
    let legacy = json!({"task":"legacy","epoch":7,"assignment":"old",
        "state_version":8,"phase":"blocked","control":"running","writer":null,
        "pending":[],"operations":{},"history":[],
        "authorization":{"deliver":{"source":"existing user agreement"}},
        "environment":{"task_id":env.task_id,"instance_id":instance,
            "workspace_root":env.workspace_root,"temp_root":env.temp_root},
        "undelivered":{"path":artifact,"sha256":util::pyjson::sha256_hex(b"existing undelivered result")},
        "unknown_resource":user_asset,
        "acceptance":{"old":{"evidence":"worker complete is not independent acceptance"}}});
    state["tasks"] = json!({"legacy":legacy});
    queue::save(&q, &mut state).unwrap();
    let spec = json!({"kind":"migrate","task":"legacy","state_version":8,
        "goal":"deliver the existing file after independent acceptance",
        "basis":{"original":["existing user agreement"],"changes":[],"specifications":[],
            "endpoint":formal,"confirmation":"existing user agreement"},
        "authorization":[{"id":"deliver","action":"delivery","source":"existing user agreement"}]});
    let workspace = env.workspace_root.clone();
    let temporary = env.temp_root.clone();
    let registry_path = temp.path().join("registry/registry.json");
    let registry_before = fs::read(&registry_path).unwrap();
    let document_before = fs::read(env.state_file()).unwrap();
    let roots_before: Vec<_> = [&workspace, &temporary]
        .iter()
        .map(|p| {
            let metadata = fs::metadata(p).unwrap();
            (metadata.dev(), metadata.ino())
        })
        .collect();
    let assert_resources_unchanged = || {
        assert_eq!(occupied_slots().unwrap(), 1);
        assert_eq!(fs::read(&registry_path).unwrap(), registry_before);
        assert_eq!(fs::read(env.state_file()).unwrap(), document_before);
        for (path, identity) in [&workspace, &temporary].iter().zip(&roots_before) {
            let metadata = fs::metadata(path).unwrap();
            assert_eq!((metadata.dev(), metadata.ino()), *identity);
        }
        assert_eq!(fs::read(&artifact).unwrap(), b"existing undelivered result");
    };
    // A rejected requirement must leave the persisted old task and real file intact.
    let before = fs::read(q.join("queue.json")).unwrap();
    let mut invalid = spec.clone();
    invalid["basis"]["original"] = json!([]);
    assert!(lineage::action(&mut state, &invalid, 1.0).is_err());
    assert_eq!(fs::read(q.join("queue.json")).unwrap(), before);
    assert_eq!(state["tasks"]["legacy"], legacy);
    assert_resources_unchanged();
    lineage::action(&mut state, &spec, 2.0).unwrap();
    queue::save(&q, &mut state).unwrap();
    // Migration only changes queue records; resource references are opaque.
    assert_resources_unchanged();
    assert!(restore_environment("legacy").is_err());
    drop(env); // Release ownership; restoring must reuse, not recreate, the roots.
    let restored = restore_environment("legacy").unwrap();
    assert_eq!(restored.instance_id, instance);
    assert_eq!(restored.workspace_root, workspace);
    assert_eq!(restored.temp_root, temporary);
    assert!(is_registered("legacy").unwrap());
    let registry = load_registry().unwrap();
    let entry = registry.get("legacy").unwrap();
    assert_eq!(entry.instance_id, restored.instance_id);
    assert_eq!(entry.workspace_root, restored.workspace_root);
    assert_eq!(entry.temp_root, restored.temp_root);
    assert_eq!(entry.state_file, restored.state_file());
    assert_eq!(entry.claimed_roots(), vec![workspace, temporary]);
    assert_eq!(occupied_slots().unwrap(), 1);
    let migrated = queue::load_queue(&q).unwrap();
    let task = &migrated["tasks"]["legacy"];
    assert_eq!(task["legacy"], legacy);
    assert!(task["approved"].is_null());
    assert!(task["candidate"].is_null());
    assert!(task["delivery_target"].is_null());
    let requirement = &task["requirements"]["1"];
    assert_eq!(requirement["basis"]["endpoint"], json!(formal));
    // New authorization is explicitly supplied by the caller, not inferred from legacy.
    assert_eq!(
        requirement["authorization"]["deliver"]["source"],
        "existing user agreement"
    );
    assert_eq!(requirement["acceptance"]["goal"]["evidence"], json!(null));
    assert!(is_delivery(&restored, &formal));
    assert!(!is_delivery(&restored, &user_asset));
    assert!(!is_within_disposable(&restored, &user_asset));
    assert!(restored
        .delivery_targets
        .iter()
        .all(|t| t.path != user_asset));
    assert!(!restored.open_files.contains(&user_asset));
    assert!(restored
        .worktree_registrations
        .iter()
        .all(|w| w.path != user_asset));
    assert_eq!(
        fs::read_to_string(&artifact).unwrap(),
        "existing undelivered result"
    );
    assert!(validate_path(&restored, &artifact).is_ok());
    assert!(validate_path(&restored, &user_asset).is_err());
    assert!(!formal.exists(), "migration must not perform delivery");
    // Explicit fixture copy tests target preservation, not product delivery acceptance.
    fs::copy(&artifact, &formal).unwrap();
    destroy_environment(&restored).unwrap();
    assert!(!restored.workspace_root.exists());
    assert!(!restored.temp_root.exists());
    assert!(!restored.state_file().exists());
    assert!(!is_registered("legacy").unwrap());
    assert_eq!(occupied_slots().unwrap(), 0);
    assert_eq!(
        fs::read_to_string(formal).unwrap(),
        "existing undelivered result"
    );
    assert_eq!(
        fs::read_to_string(user_asset).unwrap(),
        "unrelated user asset"
    );
}
