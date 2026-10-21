//! Gate configuration is protected by the persisted queue boundary.
pub mod common;
use herdr_flow::{queue, util};
use serde_json::json;
use std::{
    fs,
    os::unix::fs::{symlink, PermissionsExt},
};

#[test]
fn batch_mutation_rejects_writable_or_redirected_configuration_without_rewriting_it() {
    let root = common::tempdir().unwrap();
    let path = root.path().join("queue.json");
    let state = json!({"tasks":{"lane":{"lane":{"gate":{
        "verifier":["/bin/false"],"policy":"protected","app_id":1,"check_name":"gate"
    }}}}});
    util::atomic_write_json(&path, &state).unwrap();
    assert_eq!(queue::load_queue(root.path()).unwrap(), state);
    let original = fs::read(&path).unwrap();
    let no_adapter = |_state: &serde_json::Value| -> herdr_flow::Result<Box<dyn herdr_flow::herdr::ReceiverAdapter>> {
        panic!("unsafe configuration must be rejected before adapter use")
    };
    for mode in [0o620, 0o602, 0o666] {
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
        let error = queue::task_action(
            root.path(),
            "batch-gate-record",
            &json!({"task":"lane"}),
            None,
            &no_adapter,
        )
        .unwrap_err();
        assert!(
            error.message.contains("must not be group/world writable"),
            "{error}"
        );
        assert_eq!(fs::read(&path).unwrap(), original);
    }
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let redirected = root.path().join("redirected.json");
    fs::rename(&path, &redirected).unwrap();
    symlink(&redirected, &path).unwrap();
    assert!(queue::task_action(
        root.path(),
        "batch-gate-record",
        &json!({"task":"lane"}),
        None,
        &no_adapter
    )
    .is_err());
    assert!(path.is_symlink());
    assert_eq!(fs::read(&redirected).unwrap(), original);
}

#[test]
fn trusted_owner_can_deliberately_replace_configuration() {
    let root = common::tempdir().unwrap();
    let path = root.path().join("queue.json");
    let before = json!({"gate":{"verifier":["/bin/false"]}});
    let after = json!({"gate":{"verifier":["/bin/true"]}});
    util::atomic_write_json(&path, &before).unwrap();
    assert_eq!(queue::load_queue(root.path()).unwrap(), before);
    util::atomic_write_json(&path, &after).unwrap();
    assert_eq!(queue::load_queue(root.path()).unwrap(), after);
    // Configuration is an operator trust root: owner replacement is allowed.
    // Receipt authenticity is independently tested in gate_receipt/pr_batch.
}
