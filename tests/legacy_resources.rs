//! Legacy queue migration with an already-owned environment; no arbitrary adoption.
use herdr_flow::{progress::lineage, queue, task_env::*, util};
use serde_json::json;
use std::{fs, os::unix::fs::PermissionsExt};

#[test]
fn migration_restores_owned_resources_without_adopting_unknown_paths() {
    let temp = tempfile::Builder::new()
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir()
        .unwrap();
    std::env::set_var("HERDR_TASK_ENV_ROOT", temp.path().join("registry"));
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
    // A rejected requirement must leave the persisted old task and real file intact.
    let before = fs::read(q.join("queue.json")).unwrap();
    let mut invalid = spec.clone();
    invalid["basis"]["original"] = json!([]);
    assert!(lineage::action(&mut state, &invalid, 1.0).is_err());
    assert_eq!(fs::read(q.join("queue.json")).unwrap(), before);
    assert_eq!(state["tasks"]["legacy"], legacy);
    lineage::action(&mut state, &spec, 2.0).unwrap();
    queue::save(&q, &mut state).unwrap();
    drop(env); // Release ownership; restoring must reuse, not recreate, the roots.
    let restored = restore_environment("legacy").unwrap();
    assert_eq!(restored.instance_id, instance);
    assert_eq!(occupied_slots().unwrap(), 1);
    let migrated = queue::load_queue(&q).unwrap();
    let task = &migrated["tasks"]["legacy"];
    assert_eq!(task["legacy"], legacy);
    assert!(task["approved"].is_null());
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
