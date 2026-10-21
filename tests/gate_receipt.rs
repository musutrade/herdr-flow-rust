//! Synthetic verifier responses; no trusted service or publication side effects.
pub mod common;
use herdr_flow::{batch::gate, util};
use serde_json::{json, Value};
use std::{fs, path::Path};

fn fixture() -> (tempfile::TempDir, Value, Value, Value) {
    let root = common::tempdir().unwrap();
    let lane = json!({"repo":"owner/repo","host":"github.com","base_ref":"main","gate":{
        "verifier":["/bin/sh",root.path().join("response")],"policy":"policy-1","check_name":"gate","app_id":123
    }});
    let batch =
        json!({"base":"base","snapshot":{"head":"head","tree":"tree"},"gate":{"operation":"op-1"}});
    let receipt = json!({"version":1,"binding":gate::binding(&lane,&batch).unwrap(),"operation":"op-1",
        "valid":true,"coverage_complete":true,"app_id":123,"check_name":"gate","decision":"allow",
        "expires_at":200.0,"run":"run-1","external_id":"external-1","evidence":"evidence-1"});
    (root, lane, batch, receipt)
}

fn response(root: &Path, receipt: &Value) {
    fs::write(
        root.join("response"),
        format!(
            "cat >/dev/null\nprintf '%s' {}\n",
            util::shell_quote(&receipt.to_string())
        ),
    )
    .unwrap();
}

#[test]
fn missing_operation_cannot_be_echoed_as_a_valid_receipt() {
    let (root, lane, mut batch, mut receipt) = fixture();
    batch["gate"] = json!({});
    receipt["operation"] = Value::Null;
    response(root.path(), &receipt);
    assert!(gate::query(&lane, &batch, Some(&json!("reference")), 100.0).is_err());
    for invalid in [Value::Null, json!(""), json!(" "), json!(false), json!(1)] {
        batch["gate"]["operation"] = invalid.clone();
        receipt["operation"] = invalid.clone();
        response(root.path(), &receipt);
        let error = gate::query(&lane, &batch, Some(&json!("reference")), 100.0).unwrap_err();
        assert!(error.message.contains("trusted gate operation"));
        response(
            root.path(),
            &json!({"version":1,"binding":receipt["binding"],"operation":invalid,
            "quiescent":true,"status":"completed","evidence":"inspection"}),
        );
        let error = gate::quiescent(&lane, &batch).unwrap_err();
        assert!(error.message.contains("trusted gate operation"));
    }
}

#[test]
fn receipts_reject_changed_binding_operation_identity_and_expiry() {
    let (root, lane, batch, receipt) = fixture();
    response(root.path(), &receipt);
    assert_eq!(
        gate::query(&lane, &batch, Some(&json!("reference")), 100.0).unwrap(),
        receipt
    );
    for (field, value) in [
        ("version", json!(2)),
        ("operation", json!("other-op")),
        ("valid", json!(false)),
        ("coverage_complete", json!(false)),
        ("app_id", json!(124)),
        ("check_name", json!("other-check")),
        ("decision", json!("unknown")),
        ("expires_at", json!(100.0)),
        ("expires_at", json!("200")),
        ("run", Value::Null),
        ("external_id", json!("")),
        ("evidence", Value::Null),
    ] {
        let mut changed = receipt.clone();
        changed[field] = value;
        response(root.path(), &changed);
        assert!(
            gate::query(&lane, &batch, Some(&json!("reference")), 100.0).is_err(),
            "accepted {field}"
        );
    }
    for field in ["repo", "host", "base_ref", "base", "head", "tree", "policy"] {
        let mut changed = receipt.clone();
        changed["binding"][field] = json!("different");
        response(root.path(), &changed);
        assert!(
            gate::query(&lane, &batch, Some(&json!("reference")), 100.0).is_err(),
            "accepted binding.{field}"
        );
    }
    response(root.path(), &receipt);
    let mut another = batch.clone();
    another["gate"]["operation"] = json!("op-2");
    assert!(gate::query(&lane, &another, Some(&json!("reference")), 100.0).is_err());
    assert!(gate::query(&lane, &batch, Some(&json!("reference")), 201.0).is_err());
}
