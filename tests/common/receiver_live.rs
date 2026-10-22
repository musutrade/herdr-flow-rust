//! Opt-in real receiver proof; the driver never acknowledges or ticks the queue.
use super::*;
use herdr_flow::{
    delivery::{Executor, Observation},
    opencode::session,
    util,
};
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

struct FailureEvidence(Vec<std::path::PathBuf>);
impl Drop for FailureEvidence {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            return;
        }
        // Preserve only bounded diagnostic files, never the selected config/auth.
        let Ok(dir) = tempfile::Builder::new()
            .prefix("herdr-receiver-failure-")
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir()
        else {
            return;
        };
        for (index, source) in self.0.iter().enumerate() {
            use std::io::Read;
            if let Ok(file) = fs::File::open(source) {
                let mut bytes = Vec::new();
                if file.take(65536).read_to_end(&mut bytes).is_ok() {
                    let _ = fs::write(dir.path().join(format!("{index}.log")), bytes);
                }
            }
        }
        eprintln!(
            "bounded native failure evidence retained at {}",
            dir.keep().display()
        );
    }
}

// Explicit opt-in applies only to the disposable copy, never the selected file.
fn allow_owned_external_directory(config: &mut J, root: &Path) {
    let root = root.canonicalize().unwrap();
    let path = root.to_str().unwrap();
    assert!(!path.contains(['*', '?']), "owned path must be literal");
    let permissions = config
        .as_object_mut()
        .unwrap()
        .entry("permission")
        .or_insert_with(|| json!({}));
    let permissions = permissions
        .as_object_mut()
        .expect("scoped permission opt-in requires an object permission policy");
    let external = permissions
        .entry("external_directory")
        .or_insert_with(|| json!({}));
    let external = external
        .as_object_mut()
        .expect("scoped permission opt-in requires an object external_directory policy");
    external.insert(path.into(), json!("allow"));
    external.insert(format!("{path}/**"), json!("allow"));
}

fn ask_external_permission(config: &mut J, path: &Path) {
    let path = path.canonicalize().unwrap();
    let path = path.to_str().unwrap();
    let permissions = config
        .as_object_mut()
        .unwrap()
        .entry("permission")
        .or_insert_with(|| json!({}));
    let permissions = permissions
        .as_object_mut()
        .expect("exact permission probe requires an object permission policy");
    let external = permissions
        .entry("external_directory")
        .or_insert_with(|| json!({}));
    let external = external
        .as_object_mut()
        .expect("exact permission probe requires an object external_directory policy");
    external.insert(permission_probe_pattern(Path::new(path)), json!("ask"));
}

#[test]
fn permission_probe_policy_only_asks_for_the_dedicated_directory() {
    let temp = temp();
    let sentinel = temp.path().join("sentinel.txt");
    fs::write(&sentinel, "disposable sentinel").unwrap();
    let selected = json!({
        "permission":{"bash":"ask","external_directory":{"*":"deny","/unrelated/**":"allow"}},
        "model":"unchanged"
    });
    let mut copy = selected.clone();
    ask_external_permission(&mut copy, &sentinel);
    let path = sentinel.canonicalize().unwrap();
    assert_eq!(
        copy["permission"]["external_directory"][permission_probe_pattern(&path)],
        "ask"
    );
    assert_eq!(copy["permission"]["external_directory"]["*"], "deny");
    assert_eq!(
        copy["permission"]["external_directory"]["/unrelated/**"],
        "allow"
    );
    assert_eq!(copy["permission"]["bash"], "ask");
    assert_eq!(copy["model"], selected["model"]);
    assert_eq!(
        selected["permission"]["external_directory"]
            .as_object()
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn permission_request_identity_rejects_wrong_session_message_call_path_or_schema() {
    let sentinel = Path::new("/tmp/native-permission-probe/sentinel");
    let valid = json!({
        "id":"permission-1","sessionID":"ses-reviewer",
        "permission":"external_directory",
        "patterns":["/tmp/native-permission-probe/*"],
        "tool":{"messageID":"asst-1","callID":"call-1"}
    });
    assert_eq!(
        permission_request_fields(&valid, "ses-reviewer", sentinel).unwrap(),
        ("asst-1".to_string(), "call-1".to_string())
    );
    let old_schema = json!({
        "id":"permission-2","sessionID":"ses-reviewer",
        "type":"external_directory","pattern":"/tmp/native-permission-probe/*",
        "messageID":"asst-2","callID":"call-2"
    });
    assert_eq!(
        permission_request_fields(&old_schema, "ses-reviewer", sentinel).unwrap(),
        ("asst-2".to_string(), "call-2".to_string())
    );
    let reject = |permission: &J| {
        assert!(permission_request_fields(permission, "ses-reviewer", sentinel).is_err());
    };
    let mut wrong_session = valid.clone();
    wrong_session["sessionID"] = json!("ses-other");
    reject(&wrong_session);
    let mut wrong_message = valid.clone();
    wrong_message["tool"]["messageID"] = json!("");
    reject(&wrong_message);
    let mut wrong_call = valid.clone();
    wrong_call["tool"]["callID"] = json!("");
    reject(&wrong_call);
    let mut wrong_suffix = valid.clone();
    wrong_suffix["patterns"] = json!(["/tmp/native-permission-probe-suffix/*"]);
    reject(&wrong_suffix);
    let mut extra_pattern = valid.clone();
    extra_pattern["patterns"] = json!(["/tmp/native-permission-probe/*", "/tmp/unrelated/*"]);
    reject(&extra_pattern);
    let mut wrong_type = valid.clone();
    wrong_type["permission"] = json!("bash");
    reject(&wrong_type);
    let mut missing_schema = valid.clone();
    missing_schema.as_object_mut().unwrap().remove("permission");
    missing_schema["tool"] = json!({});
    reject(&missing_schema);
    let temp = temp();
    let sentinel = temp.path().join("sentinel");
    fs::write(&sentinel, "sentinel").unwrap();
    let scalar = json!({"permission":{"external_directory":"allow"}});
    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut scalar = scalar.clone();
        ask_external_permission(&mut scalar, &sentinel);
    }))
    .is_err());
}

#[test]
fn scoped_external_permission_preserves_other_rules_and_selected_config() {
    let temp = temp();
    let selected = json!({"permission":{"bash":"ask","external_directory":{"*":"ask","/unrelated/**":"deny"}},"model":"unchanged"});
    let mut copy = selected.clone();
    allow_owned_external_directory(&mut copy, temp.path());
    assert_eq!(copy["permission"]["bash"], "ask");
    assert_eq!(copy["permission"]["external_directory"]["*"], "ask");
    assert_eq!(
        copy["permission"]["external_directory"]["/unrelated/**"],
        "deny"
    );
    let root = temp.path().canonicalize().unwrap();
    assert_eq!(
        copy["permission"]["external_directory"][root.to_str().unwrap()],
        "allow"
    );
    assert_eq!(
        copy["permission"]["external_directory"][format!("{}/**", root.display())],
        "allow"
    );
    assert_eq!(copy["model"], selected["model"]);
    assert_eq!(
        selected["permission"]["external_directory"]
            .as_object()
            .unwrap()
            .len(),
        2
    );
}

fn acknowledged_delivery<'a>(state: &'a J, session: &str) -> Option<&'a str> {
    let deliveries = state["deliveries"].as_object()?;
    if deliveries.len() != 1 {
        return None;
    }
    let (id, delivery) = deliveries.iter().next()?;
    (delivery["phase"] == "acknowledged" && delivery["session"] == session).then_some(id.as_str())
}

#[test]
fn native_receipt_uses_durable_delivery_identity_after_watcher_moves_on() {
    let mut state = json!({"delivery_status":"waiting_for_events","deliveries":{
        "receipt":{"phase":"acknowledged","session":"sesreceiver"}}});
    assert_eq!(
        acknowledged_delivery(&state, "sesreceiver"),
        Some("receipt")
    );
    assert_eq!(acknowledged_delivery(&state, "other"), None);
    state["delivery_status"] = json!("acknowledged");
    state["deliveries"]["receipt"]["phase"] = json!("submitted");
    assert_eq!(acknowledged_delivery(&state, "sesreceiver"), None);
}

#[test]
#[ignore = "requires selected OpenCode 1.19.3 config/model and native shell permission"]
fn installed_real_opencode_receiver_readiness_and_ack() {
    run_native_receiver(false, false, false, false, false);
}

#[test]
#[ignore = "requires selected native model: real developer result and coordinator ACK/application"]
fn installed_real_opencode_developer_result_is_applied_by_native_receiver() {
    run_native_receiver(true, false, false, false, false);
}

#[test]
#[ignore = "requires real OpenCode 1.19.3 config/model and native shell permission; ACK/stall same-session recovery"]
fn installed_real_opencode_ack_stall_recovery_is_applied_by_native_receiver() {
    let _lock = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let temp = temp();
    let base = temp.path();
    let formal_target = base.join("unused-formal.git");
    let user_asset = base.join("unused-user.txt");
    run_native_receiver_in(ReceiverSpec {
        development_adapter: DevelopmentAdapter::OpenCode,
        apply_development: true,
        apply_acceptance: false,
        apply_delivery: false,
        apply_repair: false,
        apply_restart: false,
        apply_ack_stall_recovery: true,
        apply_late_acceptance: false,
        apply_unable_restore: false,
        apply_restored_review: false,
        apply_ambiguous: false,
        apply_concurrent: false,
        apply_approval_block: false,
        apply_missing_report: false,
        apply_unknown_delivery: false,
        unavailable_dependency: None,
        base,
        formal_target: &formal_target,
        user_asset: &user_asset,
        excluded: &[],
    });
}

#[test]
#[ignore = "requires real OpenCode 1.19.3 config/model and native shell permission; fixed-snapshot delayed delivery rejection"]
fn installed_real_opencode_fixed_snapshot_late_result_is_rejected_by_native_receiver() {
    let _lock = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let temp = temp();
    let base = temp.path();
    let formal_target = base.join("unused-formal.git");
    let user_asset = base.join("unused-user.txt");
    run_native_receiver_in(ReceiverSpec {
        development_adapter: DevelopmentAdapter::OpenCode,
        apply_development: true,
        apply_acceptance: true,
        apply_delivery: false,
        apply_repair: false,
        apply_restart: false,
        apply_ack_stall_recovery: false,
        apply_late_acceptance: true,
        apply_unable_restore: false,
        apply_restored_review: false,
        apply_ambiguous: false,
        apply_concurrent: false,
        apply_approval_block: false,
        apply_missing_report: false,
        apply_unknown_delivery: false,
        unavailable_dependency: None,
        base,
        formal_target: &formal_target,
        user_asset: &user_asset,
        excluded: &[],
    });
}

#[test]
#[ignore = "requires real OpenCode and an absent operator-provided reviewer dependency; unable verdict and restore continuation"]
fn installed_real_opencode_unable_restore_is_applied_by_native_receiver() {
    let _lock = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let temp = temp();
    let base = temp.path();
    let formal_target = base.join("unused-formal.git");
    let user_asset = base.join("unused-user.txt");
    run_native_receiver_in(ReceiverSpec {
        development_adapter: DevelopmentAdapter::OpenCode,
        apply_development: true,
        apply_acceptance: true,
        apply_delivery: false,
        apply_repair: false,
        apply_restart: false,
        apply_ack_stall_recovery: false,
        apply_late_acceptance: false,
        apply_unable_restore: true,
        apply_restored_review: false,
        apply_ambiguous: false,
        apply_concurrent: false,
        apply_approval_block: false,
        apply_missing_report: false,
        apply_unknown_delivery: false,
        unavailable_dependency: Some(UNAVAILABLE_DEPENDENCY),
        base,
        formal_target: &formal_target,
        user_asset: &user_asset,
        excluded: &[],
    });
}

#[test]
#[ignore = "requires real OpenCode unavailable dependency restore, installed operator fixture and fresh passing review"]
fn installed_real_opencode_restored_review_is_applied_by_native_receiver() {
    let _lock = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let temp = temp();
    let base = temp.path();
    let formal_target = base.join("unused-formal.git");
    let user_asset = base.join("unused-user.txt");
    run_native_receiver_in(ReceiverSpec {
        development_adapter: DevelopmentAdapter::OpenCode,
        apply_development: true,
        apply_acceptance: true,
        apply_delivery: false,
        apply_repair: false,
        apply_restart: false,
        apply_ack_stall_recovery: false,
        apply_late_acceptance: false,
        apply_unable_restore: true,
        apply_restored_review: true,
        apply_ambiguous: false,
        apply_concurrent: false,
        apply_approval_block: false,
        apply_missing_report: false,
        apply_unknown_delivery: false,
        unavailable_dependency: Some(UNAVAILABLE_DEPENDENCY),
        base,
        formal_target: &formal_target,
        user_asset: &user_asset,
        excluded: &[],
    });
}

#[test]
#[ignore = "requires real OpenCode reviewer and native coordinator; contradictory basis and clarification stop"]
fn installed_real_opencode_ambiguous_requirement_is_applied_by_native_receiver() {
    let _lock = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let temp = temp();
    let base = temp.path();
    let formal_target = base.join("unused-formal.git");
    let user_asset = base.join("unused-user.txt");
    run_native_receiver_in(ReceiverSpec {
        development_adapter: DevelopmentAdapter::OpenCode,
        apply_development: false,
        apply_acceptance: true,
        apply_delivery: false,
        apply_repair: false,
        apply_restart: false,
        apply_ack_stall_recovery: false,
        apply_late_acceptance: false,
        apply_unable_restore: false,
        apply_restored_review: false,
        apply_ambiguous: true,
        apply_concurrent: false,
        apply_approval_block: false,
        apply_missing_report: false,
        apply_unknown_delivery: false,
        unavailable_dependency: None,
        base,
        formal_target: &formal_target,
        user_asset: &user_asset,
        excluded: &[],
    });
}

#[test]
#[ignore = "requires two concurrent real OpenCode native turns: fixed-A reviewer and developing developer"]
fn installed_real_opencode_concurrent_developer_while_reviewer_fixed_a() {
    let _lock = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let temp = temp();
    let base = temp.path();
    let formal_target = base.join("unused-formal.git");
    let user_asset = base.join("unused-user.txt");
    run_native_receiver_in(ReceiverSpec {
        development_adapter: DevelopmentAdapter::OpenCode,
        apply_development: false,
        apply_acceptance: false,
        apply_delivery: false,
        apply_repair: false,
        apply_restart: false,
        apply_ack_stall_recovery: false,
        apply_late_acceptance: false,
        apply_unable_restore: false,
        apply_restored_review: false,
        apply_ambiguous: false,
        apply_concurrent: true,
        apply_approval_block: false,
        apply_missing_report: false,
        apply_unknown_delivery: false,
        unavailable_dependency: None,
        base,
        formal_target: &formal_target,
        user_asset: &user_asset,
        excluded: &[],
    });
}

#[test]
#[ignore = "requires a real OpenCode reviewer and a disposable exact external permission prompt; bounded blocked observation only"]
fn installed_real_opencode_approval_block_stops_without_result() {
    let _lock = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let temp = temp();
    let base = temp.path();
    let formal_target = base.join("unused-formal.git");
    let user_asset = base.join("unused-user.txt");
    run_native_receiver_in(ReceiverSpec {
        development_adapter: DevelopmentAdapter::OpenCode,
        apply_development: false,
        apply_acceptance: false,
        apply_delivery: false,
        apply_repair: false,
        apply_restart: false,
        apply_ack_stall_recovery: false,
        apply_late_acceptance: false,
        apply_unable_restore: false,
        apply_restored_review: false,
        apply_ambiguous: false,
        apply_concurrent: false,
        apply_approval_block: true,
        apply_missing_report: false,
        apply_unknown_delivery: false,
        unavailable_dependency: None,
        base,
        formal_target: &formal_target,
        user_asset: &user_asset,
        excluded: &[],
    });
}

#[test]
#[ignore = "requires real OpenCode reviewer and coordinator for a prompt-induced completed turn without verdict report"]
fn installed_real_opencode_missing_verdict_is_restored_by_native_receiver() {
    let _lock = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let temp = temp();
    let base = temp.path();
    let formal_target = base.join("unused-formal.git");
    let user_asset = base.join("unused-user.txt");
    run_native_receiver_in(ReceiverSpec {
        development_adapter: DevelopmentAdapter::OpenCode,
        apply_development: false,
        apply_acceptance: false,
        apply_delivery: false,
        apply_repair: false,
        apply_restart: false,
        apply_ack_stall_recovery: false,
        apply_late_acceptance: false,
        apply_unable_restore: false,
        apply_restored_review: false,
        apply_ambiguous: false,
        apply_concurrent: false,
        apply_approval_block: false,
        apply_missing_report: true,
        apply_unknown_delivery: false,
        unavailable_dependency: None,
        base,
        formal_target: &formal_target,
        user_asset: &user_asset,
        excluded: &[],
    });
}

#[test]
#[ignore = "requires real developer, independent reviewer and native coordinator application"]
fn installed_real_opencode_independent_acceptance_is_applied_by_native_receiver() {
    run_native_receiver(true, true, false, false, false);
}

#[test]
#[ignore = "requires actual native acceptance and external Local delivery; injected lost observation then fresh reconcile/finish"]
fn installed_real_opencode_accepted_delivery_unknown_is_reconciled_by_fresh_supervisor() {
    let _lock = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let temp = temp();
    let base = temp.path();
    let formal_target = base.join("formal.git");
    let user_asset = base.join("user-owned.txt");
    let excluded = vec![formal_target.clone(), user_asset.clone()];
    run_native_receiver_in(ReceiverSpec {
        development_adapter: DevelopmentAdapter::OpenCode,
        apply_development: true,
        apply_acceptance: true,
        apply_delivery: true,
        apply_repair: false,
        apply_restart: false,
        apply_ack_stall_recovery: false,
        apply_late_acceptance: false,
        apply_unable_restore: false,
        apply_restored_review: false,
        apply_ambiguous: false,
        apply_concurrent: false,
        apply_approval_block: false,
        apply_missing_report: false,
        apply_unknown_delivery: true,
        unavailable_dependency: None,
        base,
        formal_target: &formal_target,
        user_asset: &user_asset,
        excluded: &excluded,
    });
}

#[test]
#[ignore = "requires accepted native candidate and external unmarked Local Git delivery"]
fn installed_real_opencode_accepted_result_is_delivered_by_external_driver() {
    run_native_receiver(true, true, true, false, false);
}

#[test]
#[ignore = "requires real failed review, repair, fresh review and external Local Git delivery"]
fn installed_real_opencode_failed_review_is_repaired_and_delivered_by_external_driver() {
    run_native_receiver(true, true, true, true, false);
}

#[test]
#[ignore = "requires confirmed external delivery and fresh unmarked supervisor restart"]
fn installed_real_opencode_confirmed_delivery_restarts_in_external_supervisor() {
    run_native_receiver(true, true, true, true, true);
}

#[test]
#[ignore = "requires a real task-owned Codex developer, real OpenCode coordinator/reviewer and Local delivery"]
fn installed_real_codex_developer_with_opencode_coordinator_and_delivery() {
    acceptance_live::require_nested_userns();
    let temp = temp();
    let base = temp.path();
    let formal_target = base.join("formal.git");
    let user_asset = base.join("user-owned.txt");
    let excluded = vec![formal_target.clone(), user_asset.clone()];
    let _lock = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    run_native_receiver_in(ReceiverSpec {
        development_adapter: DevelopmentAdapter::Codex,
        apply_development: true,
        apply_acceptance: true,
        apply_delivery: true,
        apply_repair: false,
        apply_restart: false,
        apply_ack_stall_recovery: false,
        apply_late_acceptance: false,
        apply_unable_restore: false,
        apply_restored_review: false,
        apply_ambiguous: false,
        apply_concurrent: false,
        apply_approval_block: false,
        apply_missing_report: false,
        apply_unknown_delivery: false,
        unavailable_dependency: None,
        base,
        formal_target: &formal_target,
        user_asset: &user_asset,
        excluded: &excluded,
    });
}

#[test]
#[ignore = "requires two completed real native rounds: normal delivery then repair/restart on one outer base"]
fn installed_real_opencode_two_rounds_share_one_outer_base() {
    let temp = temp();
    let base = temp.path();
    let rounds = vec![
        campaign_round(base, 1, false),
        campaign_round(base, 2, true),
    ];
    run_campaign(base, &rounds);
}

#[test]
#[ignore = "requires twenty completed real native rounds on one outer base"]
fn installed_real_opencode_twenty_rounds_share_one_outer_base() {
    let temp = temp();
    let base = temp.path();
    let rounds = twenty_round_plans(base);
    run_campaign(base, &rounds);
}

#[test]
fn twenty_round_schedule_and_summary_bounds_are_fixed() {
    let base = Path::new("/tmp/herdr-native-twenty-schedule");
    let rounds = twenty_round_plans(base);
    assert_eq!(rounds.len(), 20);
    assert_eq!(rounds.iter().filter(|round| round.repair).count(), 4);
    let repair_indices: Vec<_> = rounds
        .iter()
        .filter(|round| round.repair)
        .map(|round| round.index)
        .collect();
    assert_eq!(repair_indices, vec![5, 10, 15, 20]);
    let mut paths = std::collections::BTreeSet::new();
    for round in &rounds {
        assert!(paths.insert(round.formal.clone()));
        assert!(paths.insert(round.user.clone()));
    }
    assert_eq!(paths.len(), 40);
    let records: Vec<J> = (1..=20)
        .map(|index| {
            json!({
                "index": index,
                "completed": true,
                "expected_artifact": "artifact",
                "output": {"commit": "artifact"},
                "cleanup": {"registry_slots": 0, "owned_roots": 0, "reports": 2}
            })
        })
        .collect();
    let document = summary_document(&records);
    assert_eq!(document["records"].as_array().unwrap().len(), 20);
    assert!(serde_json::to_vec(&document).unwrap().len() <= 256 * 1024);

    // Synthetic boundary checks: no failed record, no 21st record, no oversized summary.
    let completed = json!({"completed": true});
    let mut bounded = Vec::new();
    assert!(append_summary_record(&mut bounded, completed.clone()).is_ok());
    assert!(append_summary_record(&mut bounded, json!({"completed": false})).is_err());
    for _ in 1..20 {
        append_summary_record(&mut bounded, completed.clone()).unwrap();
    }
    assert_eq!(bounded.len(), 20);
    assert!(append_summary_record(&mut bounded, completed).is_err());
    let oversized = json!({"completed": true, "padding": "x".repeat(300 * 1024)});
    let mut oversized_records = Vec::new();
    assert!(append_summary_record(&mut oversized_records, oversized).is_err());
    let temp = temp();
    let path = temp.path().join("summary.json");
    write_summary(&path, &[]);
    let previous = fs::read(&path).unwrap();
    let invalid = json!({"index": 1, "completed": true, "expected_artifact": "a",
        "output": {"commit": "wrong"},
        "cleanup": {"registry_slots": 0, "owned_roots": 0, "reports": 2}});
    assert!(std::panic::catch_unwind(|| write_summary(&path, &[invalid])).is_err());
    assert_eq!(fs::read(&path).unwrap(), previous);
}

fn user_identity(path: &Path) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    let metadata = fs::metadata(path).unwrap();
    (metadata.dev(), metadata.ino())
}

fn assert_round_output(target: &Path, user_asset: &Path, commit: &str, identity: (u64, u64)) {
    use std::os::unix::fs::MetadataExt;
    assert_eq!(git(target, &["rev-parse", "refs/heads/main"]), commit);
    assert_eq!(git(target, &["show", "refs/heads/main:file"]), "version B");
    let metadata = fs::metadata(user_asset).unwrap();
    assert_eq!((metadata.dev(), metadata.ino()), identity);
    assert_eq!(
        fs::read_to_string(user_asset).unwrap(),
        "unrelated user asset"
    );
}

fn sorted_names(path: &Path) -> Result<Vec<String>, String> {
    let entries =
        fs::read_dir(path).map_err(|error| format!("read_dir {}: {error}", path.display()))?;
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| format!("entry {}: {error}", path.display()))?;
        names.push(entry.file_name().to_string_lossy().into_owned());
    }
    names.sort();
    Ok(names)
}

fn report_pair_metadata(base: &Path) -> (usize, u64) {
    let names = sorted_names(base).unwrap();
    let reports: Vec<&str> = names
        .iter()
        .filter(|name| name.starts_with("inventory-"))
        .map(String::as_str)
        .collect();
    assert_eq!(
        reports,
        vec!["inventory-after.json", "inventory-before.json"],
        "reports must be one rolling pair"
    );
    let mut bytes = 0u64;
    for report in &reports {
        let path = base.join(report);
        let len = fs::metadata(&path).unwrap().len();
        assert!(len > 0 && len <= 1024 * 1024, "unbounded report {report}");
        bytes += len;
    }
    (reports.len(), bytes)
}

fn compact_base(report: &J) -> J {
    let base = &report["base_usage"];
    json!({
        "entries": base["entries"],
        "files": base["files"],
        "dirs": base["dirs"],
        "symlinks": base["symlinks"],
        "bytes": base["bytes"],
        "allocated_blocks_512": base["allocated_blocks_512"],
        "excluded_files": base["excluded_files"],
        "excluded_bytes": base["excluded_bytes"],
        "excluded_allocated_blocks_512": base["excluded_allocated_blocks_512"],
        "truncated": base["truncated"],
    })
}

fn incomplete_processes(report: &J) -> usize {
    report["processes"]["records"]
        .as_array()
        .expect("missing process records")
        .iter()
        .filter(|record| record["complete"] != true)
        .count()
}

fn summary_bytes(records: &[J]) -> Vec<u8> {
    serde_json::to_vec_pretty(&summary_document(records)).unwrap()
}

fn control_footprint(path: &Path, root: &Path, out: &mut Vec<String>) -> Result<(), String> {
    for name in sorted_names(path)? {
        let child = path.join(name);
        let metadata = fs::symlink_metadata(&child)
            .map_err(|error| format!("control metadata {}: {error}", child.display()))?;
        let kind = if metadata.is_dir() {
            "dir"
        } else if metadata.is_file() {
            "file"
        } else {
            "other"
        };
        let relative = child
            .strip_prefix(root)
            .map_err(|error| error.to_string())?
            .display()
            .to_string();
        out.push(format!("{relative}:{kind}"));
        if metadata.is_dir() {
            control_footprint(&child, root, out)?;
        }
    }
    Ok(())
}

fn retained_control(base: &Path, rounds: &[&str]) -> Result<Vec<String>, String> {
    let mut expected = vec![
        "campaign-summary.json".to_string(),
        "inventory-after.json".to_string(),
        "inventory-before.json".to_string(),
        "registry".to_string(),
    ];
    expected.extend(rounds.iter().map(|round| (*round).to_string()));
    expected.sort();
    if sorted_names(base)? != expected {
        return Err(format!(
            "unexpected retained base entries under {}",
            base.display()
        ));
    }
    for round in rounds {
        let round = base.join(*round);
        let expected = vec!["formal.git".to_string(), "user-owned.txt".to_string()];
        if sorted_names(&round)? != expected {
            return Err(format!(
                "unexpected retained round entries under {}",
                round.display()
            ));
        }
    }
    for owned in ["workspace", "temporary"] {
        if base.join(owned).exists() {
            return Err(format!("owned root {} remains", base.join(owned).display()));
        }
    }
    let registry = base.join("registry");
    let expected = vec![
        "envs".to_string(),
        "locks".to_string(),
        "plans".to_string(),
        "registry.json".to_string(),
        "registry.lock".to_string(),
    ];
    if sorted_names(&registry)? != expected {
        return Err(format!(
            "unexpected shared registry entries under {}",
            registry.display()
        ));
    }
    for subdir in ["envs", "plans", "locks"] {
        let names = sorted_names(&registry.join(subdir))?;
        if !names.is_empty() {
            return Err(format!(
                "retained {subdir} entries under {}: {names:?}",
                registry.display()
            ));
        }
    }
    let index: J = serde_json::from_slice(
        &fs::read(registry.join("registry.json"))
            .map_err(|error| format!("read registry index: {error}"))?,
    )
    .map_err(|error| format!("parse registry index: {error}"))?;
    let entries = index
        .get("entries")
        .and_then(J::as_object)
        .ok_or_else(|| "registry index entries missing".to_string())?;
    if !entries.is_empty() {
        return Err("registry index retains entries".to_string());
    }
    let mut footprint = Vec::new();
    control_footprint(&registry, &registry, &mut footprint)?;
    Ok(footprint)
}

struct CampaignRound {
    index: usize,
    repair: bool,
    formal: PathBuf,
    user: PathBuf,
}

fn campaign_round(base: &Path, index: usize, repair: bool) -> CampaignRound {
    let directory = base.join(format!("round-{index}"));
    CampaignRound {
        index,
        repair,
        formal: directory.join("formal.git"),
        user: directory.join("user-owned.txt"),
    }
}

fn twenty_round_plans(base: &Path) -> Vec<CampaignRound> {
    (1..=20)
        .map(|index| campaign_round(base, index, index % 5 == 0))
        .collect()
}

struct CompletedRound {
    formal: PathBuf,
    user: PathBuf,
    commit: String,
    identity: (u64, u64),
}

fn summary_document(records: &[J]) -> J {
    json!({"schema": 1, "max_records": 20, "records": records})
}

fn append_summary_record(records: &mut Vec<J>, record: J) -> Result<(), String> {
    if record["completed"] != true {
        return Err("only completed records may be appended".to_string());
    }
    let mut candidate = records.clone();
    candidate.push(record);
    if candidate.len() > 20 {
        return Err("summary exceeds twenty records".to_string());
    }
    if summary_bytes(&candidate).len() > 256 * 1024 {
        return Err("summary exceeds 256KiB".to_string());
    }
    *records = candidate;
    Ok(())
}

fn write_summary(path: &Path, records: &[J]) {
    assert!(records.len() <= 20, "summary exceeds twenty records");
    assert_summary(&summary_document(records), records.len());
    let bytes = summary_bytes(records);
    assert!(bytes.len() <= 256 * 1024, "summary exceeds 256KiB");
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, bytes).unwrap();
    fs::rename(temporary, path).unwrap();
}

fn assert_summary(document: &J, expected: usize) {
    assert_eq!(document["max_records"], 20);
    let records = document["records"].as_array().unwrap();
    assert_eq!(records.len(), expected);
    for (index, record) in records.iter().enumerate() {
        assert_eq!(record["index"], json!(index + 1));
        assert_eq!(record["completed"], true);
        assert!(record["expected_artifact"]
            .as_str()
            .is_some_and(|value| !value.is_empty()));
        assert_eq!(record["output"]["commit"], record["expected_artifact"]);
        assert_eq!(record["cleanup"]["registry_slots"], 0);
        assert_eq!(record["cleanup"]["owned_roots"], 0);
        assert_eq!(record["cleanup"]["reports"], 2);
    }
}

fn run_campaign(base: &Path, rounds: &[CampaignRound]) {
    assert!(!rounds.is_empty() && rounds.len() <= 20);
    let _lock = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let excluded: Vec<PathBuf> = rounds
        .iter()
        .flat_map(|round| [round.formal.clone(), round.user.clone()])
        .collect();
    let summary_path = base.join("campaign-summary.json");
    write_summary(&summary_path, &[]);
    let _campaign_evidence = FailureEvidence(vec![
        base.join("inventory-before.json"),
        base.join("inventory-after.json"),
        summary_path.clone(),
    ]);
    let mut completed: Vec<CompletedRound> = Vec::new();
    let mut records: Vec<J> = Vec::new();
    let mut control: Option<Vec<String>> = None;
    for (position, round) in rounds.iter().enumerate() {
        assert_eq!(round.index, position + 1);
        common::create_dir(base.join(format!("round-{}", round.index))).unwrap();
        eprintln!(
            "native campaign round {}/{} starting: {}",
            round.index,
            rounds.len(),
            if round.repair {
                "repair_restart"
            } else {
                "normal"
            }
        );
        let started = Instant::now();
        run_native_receiver_in(ReceiverSpec {
            development_adapter: DevelopmentAdapter::OpenCode,
            apply_development: true,
            apply_acceptance: true,
            apply_delivery: true,
            apply_repair: round.repair,
            apply_restart: round.repair,
            apply_ack_stall_recovery: false,
            apply_late_acceptance: false,
            apply_unable_restore: false,
            apply_restored_review: false,
            apply_ambiguous: false,
            apply_concurrent: false,
            apply_approval_block: false,
            apply_missing_report: false,
            apply_unknown_delivery: false,
            unavailable_dependency: None,
            base,
            formal_target: &round.formal,
            user_asset: &round.user,
            excluded: &excluded,
        });
        let elapsed_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;

        let before: J =
            serde_json::from_slice(&fs::read(base.join("inventory-before.json")).unwrap()).unwrap();
        let after: J =
            serde_json::from_slice(&fs::read(base.join("inventory-after.json")).unwrap()).unwrap();
        assert_eq!(before["expected_identity"], after["expected_identity"]);
        let expected_artifact = before["expected_identity"]["candidate"]
            .as_str()
            .expect("missing expected artifact");
        let actual_ref = git(&round.formal, &["rev-parse", "refs/heads/main"]);
        assert_eq!(actual_ref, expected_artifact);
        let identity = user_identity(&round.user);
        completed.push(CompletedRound {
            formal: round.formal.clone(),
            user: round.user.clone(),
            commit: actual_ref.clone(),
            identity,
        });
        for prior in &completed {
            assert_round_output(&prior.formal, &prior.user, &prior.commit, prior.identity);
        }

        let before_marked = before["processes"]["count"]
            .as_u64()
            .expect("missing process count");
        let after_marked = after["processes"]["count"]
            .as_u64()
            .expect("missing process count");
        assert_eq!(after_marked, 0);
        let (report_count, report_bytes) = report_pair_metadata(base);
        let names: Vec<String> = rounds[..=position]
            .iter()
            .map(|round| format!("round-{}", round.index))
            .collect();
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        let current = retained_control(base, &names).unwrap();
        if let Some(reference) = &control {
            assert_eq!(reference, &current, "shared control footprint changed");
        } else {
            control = Some(current);
        }
        let slots = occupied_slots().unwrap();
        assert_eq!(slots, 0);
        assert!(!is_registered("project").unwrap());
        let owned_roots = ["workspace", "temp"]
            .iter()
            .filter(|root| after["roots"][root]["exists"] == true)
            .count();
        assert_eq!(owned_roots, 0);

        let record = json!({
            "index": round.index,
            "scenario": if round.repair { "repair_restart" } else { "normal" },
            "completed": true,
            "expected_artifact": expected_artifact,
            "output": {
                "ref": "refs/heads/main",
                "commit": actual_ref,
                "content": "version B",
            },
            "user_identity": {
                "dev": identity.0,
                "ino": identity.1,
                "bytes": fs::metadata(&round.user).unwrap().len(),
            },
            "elapsed_ms": elapsed_ms,
            "inventory": {
                "before": {
                    "marked": before_marked,
                    "process_truncated": before["processes"]["truncated"],
                    "incomplete_processes": incomplete_processes(&before),
                    "base": compact_base(&before),
                },
                "after": {
                    "marked": after_marked,
                    "process_truncated": after["processes"]["truncated"],
                    "incomplete_processes": incomplete_processes(&after),
                    "base": compact_base(&after),
                },
                "roots": after["roots"],
                "runtime_roots": after["runtime_roots"],
                "open_files": after["open_files"],
                "worktrees": after["worktrees"],
                "state_exists": after["state"]["exists"],
                "cleanup_exists": after["cleanup"]["exists"],
                "registry": {
                    "registered": after["registry"]["registered"],
                    "entry_null": after["registry"]["entry"].is_null(),
                    "slots": after["registry"]["occupied_slots"],
                },
                "queue_absent": after["queue_entries"]["error"].is_string(),
            },
            "cleanup": {
                "owned_roots": owned_roots,
                "registry_slots": slots,
                "reports": report_count,
                "report_bytes": report_bytes,
                "completed_rounds": records.len() + 1,
            }
        });
        let mut candidate = records.clone();
        append_summary_record(&mut candidate, record).unwrap();
        assert_summary(&summary_document(&candidate), candidate.len());
        write_summary(&summary_path, &candidate);
        records = candidate;
        eprintln!("native campaign round {}/{} completed: elapsed_ms={elapsed_ms}, marked_before={before_marked}, marked_after={after_marked}, slots={slots}, owned_roots={owned_roots}, reports={report_count}, report_bytes={report_bytes}", round.index, rounds.len());
    }
    assert_eq!(
        records.len(),
        rounds.len(),
        "campaign did not complete all rounds"
    );
    assert!(!is_registered("project").unwrap());
    assert_eq!(occupied_slots().unwrap(), 0);
}

#[test]
fn retained_control_rejects_unexpected_entries() {
    let temp = temp();
    let base = temp.path().join("base");
    common::create_dir(&base).unwrap();
    for round in ["round-1", "round-2"] {
        let round = base.join(round);
        common::create_dir(&round).unwrap();
        common::create_dir(round.join("formal.git")).unwrap();
        fs::write(round.join("user-owned.txt"), "unrelated user asset").unwrap();
    }
    fs::write(
        base.join("campaign-summary.json"),
        r#"{"schema":1,"max_records":20,"records":[]}"#,
    )
    .unwrap();
    fs::write(base.join("inventory-before.json"), "{}").unwrap();
    fs::write(base.join("inventory-after.json"), "{}").unwrap();
    let registry = base.join("registry");
    common::create_dir(&registry).unwrap();
    for subdir in ["envs", "plans", "locks"] {
        common::create_dir(registry.join(subdir)).unwrap();
    }
    fs::write(registry.join("registry.json"), r#"{"entries":{}}"#).unwrap();
    fs::write(registry.join("registry.lock"), "").unwrap();
    assert!(retained_control(&base, &["round-1", "round-2"]).is_ok());
    fs::write(base.join("unexpected-retained"), "leak").unwrap();
    assert!(retained_control(&base, &["round-1", "round-2"]).is_err());
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DevelopmentAdapter {
    OpenCode,
    Codex,
}

struct ReceiverSpec<'a> {
    development_adapter: DevelopmentAdapter,
    apply_development: bool,
    apply_acceptance: bool,
    apply_delivery: bool,
    apply_repair: bool,
    apply_restart: bool,
    apply_ack_stall_recovery: bool,
    apply_late_acceptance: bool,
    apply_unable_restore: bool,
    apply_restored_review: bool,
    apply_ambiguous: bool,
    apply_concurrent: bool,
    apply_approval_block: bool,
    apply_missing_report: bool,
    apply_unknown_delivery: bool,
    unavailable_dependency: Option<&'a str>,
    base: &'a Path,
    formal_target: &'a Path,
    user_asset: &'a Path,
    excluded: &'a [PathBuf],
}

fn run_native_receiver(
    apply_development: bool,
    apply_acceptance: bool,
    apply_delivery: bool,
    apply_repair: bool,
    apply_restart: bool,
) {
    let _lock = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let temp = temp();
    let formal_target = temp.path().join("formal.git");
    let user_asset = temp.path().join("user-owned.txt");
    let excluded = vec![formal_target.clone(), user_asset.clone()];
    run_native_receiver_in(ReceiverSpec {
        development_adapter: DevelopmentAdapter::OpenCode,
        apply_development,
        apply_acceptance,
        apply_delivery,
        apply_repair,
        apply_restart,
        apply_ack_stall_recovery: false,
        apply_late_acceptance: false,
        apply_unable_restore: false,
        apply_restored_review: false,
        apply_ambiguous: false,
        apply_concurrent: false,
        apply_approval_block: false,
        apply_missing_report: false,
        apply_unknown_delivery: false,
        unavailable_dependency: None,
        base: temp.path(),
        formal_target: &formal_target,
        user_asset: &user_asset,
        excluded: &excluded,
    });
}

fn run_native_receiver_in(spec: ReceiverSpec<'_>) {
    let ReceiverSpec {
        development_adapter,
        apply_development,
        apply_acceptance,
        apply_delivery,
        apply_repair,
        apply_restart,
        apply_ack_stall_recovery,
        apply_late_acceptance,
        apply_unable_restore,
        apply_restored_review,
        apply_ambiguous,
        apply_concurrent,
        apply_approval_block,
        apply_missing_report,
        apply_unknown_delivery,
        unavailable_dependency,
        base,
        formal_target,
        user_asset,
        excluded,
    } = spec;
    assert!(
        !apply_delivery || apply_acceptance,
        "delivery requires the native acceptance stage"
    );
    assert!(
        !apply_repair || (apply_acceptance && apply_delivery),
        "repair requires acceptance and external delivery"
    );
    assert!(
        !apply_restart || (apply_repair && apply_delivery),
        "restart requires repair/delivery"
    );
    assert!(
        !apply_ack_stall_recovery
            || (development_adapter == DevelopmentAdapter::OpenCode
                && apply_development
                && !apply_acceptance
                && !apply_delivery
                && !apply_repair
                && !apply_restart),
        "ACK/stall recovery requires OpenCode development-only mode"
    );
    assert!(
        !apply_late_acceptance
            || (development_adapter == DevelopmentAdapter::OpenCode
                && apply_development
                && apply_acceptance
                && !apply_delivery
                && !apply_repair
                && !apply_restart
                && !apply_ack_stall_recovery),
        "late acceptance requires OpenCode development+acceptance mode"
    );
    assert!(
        !apply_unable_restore
            || (development_adapter == DevelopmentAdapter::OpenCode
                && apply_development
                && apply_acceptance
                && !apply_delivery
                && !apply_repair
                && !apply_restart
                && !apply_ack_stall_recovery
                && !apply_late_acceptance
                && unavailable_dependency.is_some()),
        "unable restore requires OpenCode development+acceptance with a dependency"
    );
    assert!(
        !apply_restored_review
            || (apply_unable_restore
                && development_adapter == DevelopmentAdapter::OpenCode
                && apply_development
                && apply_acceptance
                && !apply_delivery
                && !apply_repair
                && !apply_restart
                && !apply_ack_stall_recovery
                && !apply_late_acceptance
                && unavailable_dependency.is_some()),
        "restored review requires the phase-1 unable mode"
    );
    assert!(
        !apply_ambiguous
            || (development_adapter == DevelopmentAdapter::OpenCode
                && !apply_development
                && apply_acceptance
                && !apply_delivery
                && !apply_repair
                && !apply_restart
                && !apply_ack_stall_recovery
                && !apply_late_acceptance
                && !apply_unable_restore
                && !apply_restored_review
                && unavailable_dependency.is_none()),
        "ambiguous requirement requires acceptance-only OpenCode mode"
    );
    assert!(
        !apply_concurrent
            || (development_adapter == DevelopmentAdapter::OpenCode
                && !apply_development
                && !apply_acceptance
                && !apply_delivery
                && !apply_repair
                && !apply_restart
                && !apply_ack_stall_recovery
                && !apply_late_acceptance
                && !apply_unable_restore
                && !apply_restored_review
                && !apply_ambiguous
                && unavailable_dependency.is_none()),
        "concurrent native turns require isolated OpenCode mode"
    );
    assert!(
        !apply_approval_block
            || (development_adapter == DevelopmentAdapter::OpenCode
                && !apply_development
                && !apply_acceptance
                && !apply_delivery
                && !apply_repair
                && !apply_restart
                && !apply_ack_stall_recovery
                && !apply_late_acceptance
                && !apply_unable_restore
                && !apply_restored_review
                && !apply_ambiguous
                && !apply_concurrent
                && unavailable_dependency.is_none()),
        "approval block requires isolated OpenCode mode"
    );
    assert!(
        !apply_missing_report
            || (development_adapter == DevelopmentAdapter::OpenCode
                && !apply_development
                && !apply_acceptance
                && !apply_delivery
                && !apply_repair
                && !apply_restart
                && !apply_ack_stall_recovery
                && !apply_late_acceptance
                && !apply_unable_restore
                && !apply_restored_review
                && !apply_ambiguous
                && !apply_concurrent
                && !apply_approval_block
                && unavailable_dependency.is_none()),
        "missing-report execution_problem requires isolated OpenCode mode"
    );
    assert!(
        !apply_unknown_delivery
            || (development_adapter == DevelopmentAdapter::OpenCode
                && apply_development
                && apply_acceptance
                && apply_delivery
                && !apply_repair
                && !apply_restart
                && !apply_ack_stall_recovery
                && !apply_late_acceptance
                && !apply_unable_restore
                && !apply_restored_review
                && !apply_ambiguous
                && !apply_concurrent
                && !apply_approval_block
                && !apply_missing_report
                && unavailable_dependency.is_none()),
        "unknown delivery requires native development/acceptance and external delivery"
    );
    let selected_config_before = (apply_concurrent
        || apply_approval_block
        || apply_missing_report
        || apply_unknown_delivery)
        .then(|| fs::read(std::env::var("HERDR_LIVE_OPENCODE_CONFIG").unwrap()).unwrap());
    if apply_delivery {
        fs::write(user_asset, "unrelated user asset").unwrap();
    }
    use std::os::unix::fs::MetadataExt;
    let user_identity = apply_delivery.then(|| {
        let m = fs::metadata(user_asset).unwrap();
        (m.dev(), m.ino())
    });
    let mut env = if apply_delivery {
        environment_project(base, formal_target)
    } else {
        environment(base)
    };
    let cwd = env.workspace_root.join("receiver");
    fs::create_dir(&cwd).unwrap();
    let root = task_runtime::prepare(&env, "real-receiver").unwrap();
    let worker_root = (apply_development || apply_missing_report)
        .then(|| task_runtime::prepare(&env, "review").unwrap());
    let reviewer_name = if apply_repair {
        "accept-first"
    } else {
        "accept-native"
    };
    let reviewer_root =
        apply_acceptance.then(|| task_runtime::prepare(&env, reviewer_name).unwrap());
    let restored_reviewer_root =
        apply_restored_review.then(|| task_runtime::prepare(&env, "accept-restored").unwrap());
    let repair_root = apply_repair.then(|| task_runtime::prepare(&env, "repair-native").unwrap());
    let final_reviewer_root =
        apply_repair.then(|| task_runtime::prepare(&env, "accept-final").unwrap());
    let mut runtime_roots = vec![root.clone()];
    for runtime in [
        &worker_root,
        &reviewer_root,
        &restored_reviewer_root,
        &repair_root,
        &final_reviewer_root,
    ]
    .into_iter()
    .flatten()
    {
        runtime_roots.push(runtime.clone());
    }
    let expected_tasks = if apply_repair {
        vec![
            "accept-final".to_string(),
            "accept-first".to_string(),
            "project".to_string(),
            "repair-native".to_string(),
            "review".to_string(),
        ]
    } else if apply_ambiguous {
        vec!["accept-native".to_string(), "project".to_string()]
    } else if apply_concurrent {
        vec![
            "accept-a".to_string(),
            "develop-b".to_string(),
            "project".to_string(),
        ]
    } else if apply_approval_block || apply_missing_report {
        vec!["project".to_string(), "review".to_string()]
    } else {
        vec![
            "accept-native".to_string(),
            "project".to_string(),
            "review".to_string(),
        ]
    };
    let notifications = if apply_repair {
        4
    } else if apply_ambiguous || apply_approval_block || apply_missing_report {
        1
    } else {
        2
    };
    let mut artifact_names = if apply_ambiguous {
        vec!["source", "reviewer-checkout"]
    } else if apply_concurrent {
        vec!["source", "accept-a", "develop-b"]
    } else if apply_approval_block || apply_missing_report {
        vec!["source", "acceptance"]
    } else {
        vec!["source", "development", "reviewer-checkout"]
    };
    if apply_repair {
        artifact_names.extend(["repair-checkout", "final-reviewer-checkout"]);
    }
    if apply_late_acceptance {
        artifact_names.extend(["late-reviewer-checkout", "late-candidate"]);
    }
    if apply_restored_review {
        artifact_names.push("restored-reviewer-checkout");
    }
    let artifact_roots: Vec<_> = if apply_development
        || apply_ambiguous
        || apply_concurrent
        || apply_approval_block
        || apply_missing_report
    {
        artifact_names
            .iter()
            .map(|name| {
                let path = env.workspace_root.join(name);
                common::create_dir_all(&path).unwrap();
                path
            })
            .collect()
    } else {
        Vec::new()
    };
    // Native launch restricts writes to this runtime root and cwd. Coordinator
    // state must live inside that boundary, not in a sibling of the runtime.
    let q = root.join("queue");
    let guard = root.join("guard");
    let developer_release = root.join("developer-release.json");
    let repair_release = root.join("repair-native-release.json");
    let mut evidence_paths = vec![
        root.join("server.log"),
        guard.join("foreground.log"),
        q.join("foreground.log"),
        q.join("queue.json"),
        q.join("readiness.json"),
        guard.join("submission.json"),
    ];
    if let Some(worker_root) = &worker_root {
        evidence_paths.extend([
            worker_root.join("server.log"),
            worker_root.join("submission.json"),
        ]);
    }
    if let Some(reviewer_root) = &reviewer_root {
        evidence_paths.extend([
            reviewer_root.join("server.log"),
            reviewer_root.join("submission.json"),
        ]);
    }
    if let Some(restored_reviewer_root) = &restored_reviewer_root {
        evidence_paths.extend([
            restored_reviewer_root.join("server.log"),
            restored_reviewer_root.join("submission.json"),
            env.workspace_root
                .join("restored-reviewer-checkout/.herdr-verdict.json"),
            q.parent().unwrap().join("restore-dependency-install.json"),
            q.parent().unwrap().join("restore-tool-evidence.json"),
        ]);
    }
    if let Some(repair_root) = &repair_root {
        evidence_paths.extend([
            repair_root.join("server.log"),
            repair_root.join("submission.json"),
        ]);
    }
    if let Some(final_reviewer_root) = &final_reviewer_root {
        evidence_paths.extend([
            final_reviewer_root.join("server.log"),
            final_reviewer_root.join("submission.json"),
        ]);
    }
    if apply_delivery {
        evidence_paths.extend([
            base.join("inventory-before.json"),
            base.join("inventory-after.json"),
            base.join("campaign-summary.json"),
        ]);
    }
    if apply_ack_stall_recovery {
        evidence_paths.push(base.join("ack-only-snapshot.json"));
    }
    if apply_unable_restore {
        evidence_paths.push(
            env.workspace_root
                .join("reviewer-checkout/.herdr-verdict.json"),
        );
    }
    if apply_ambiguous {
        evidence_paths.push(
            env.workspace_root
                .join("reviewer-checkout/.herdr-verdict.json"),
        );
    }
    let mut failure_evidence = FailureEvidence(evidence_paths);
    let selected = std::env::var("HERDR_LIVE_OPENCODE_CONFIG").unwrap();
    let mut config = util::read_json(Path::new(&selected)).unwrap();
    // Only an explicit operator opt-in may widen the disposable copy's directory
    // permission. All other native approvals still block this bounded test.
    if std::env::var("HERDR_LIVE_ALLOW_OWNED_EXTERNAL_DIR").as_deref() == Ok("1") {
        allow_owned_external_directory(&mut config, &root);
        if let Some(worker_root) = &worker_root {
            // The coordinator reads the real collector report, but native kernel
            // confinement still prevents writes to this sibling worker runtime.
            allow_owned_external_directory(&mut config, worker_root);
        }
        if let Some(reviewer_root) = &reviewer_root {
            allow_owned_external_directory(&mut config, reviewer_root);
        }
        if let Some(restored_reviewer_root) = &restored_reviewer_root {
            allow_owned_external_directory(&mut config, restored_reviewer_root);
        }
        if let Some(repair_root) = &repair_root {
            allow_owned_external_directory(&mut config, repair_root);
        }
        if let Some(final_reviewer_root) = &final_reviewer_root {
            allow_owned_external_directory(&mut config, final_reviewer_root);
        }
        for artifact_root in &artifact_roots {
            // Read-only to this coordinator at the kernel layer; these are
            // exact disposable test checkouts, never operator repositories.
            allow_owned_external_directory(&mut config, artifact_root);
        }
    }
    let instructions = cwd.join("receiver-instructions.md");
    let late_application = cwd.join("late-application.json");
    let binary = util::shell_quote(env!("CARGO_BIN_EXE_herdr-flow"));
    let queue_path = util::shell_quote(q.to_str().unwrap());
    let mut instruction_text = format!(
        "This is an authorized disposable receiver integration test. Do not delegate or create sessions. Upon receiving the queue notification, use its exact OPENCODE_SESSION_ID to execute the exact ACK command in that notification first. If it succeeds, run {binary} queue readiness --state-dir {queue_path} and require active_wakeup=true. An unacknowledged active delivery intentionally makes readiness false; do not treat that as permission to edit state. Retry only lock-busy errors for at most 15 seconds. Do not fabricate JSON or edit queue/watch files. ACK means receipt only. Then finish your turn. No other task work is authorized."
    );
    if apply_ack_stall_recovery {
        instruction_text = format!(
            "Authorized disposable coordinator test for ACK/stall recovery. This is same-session omission recovery, not a process restart. Run each queue ACK, event-current, task-apply and readiness command in its own separate bash tool invocation; do not group these commands into a script or a single shell call. Do not delegate or create sessions. Exactly one real development result for task review will arrive. First notification: use its exact OPENCODE_SESSION_ID, execute its exact queue ack command, require success, then finish immediately. Do not run task-apply, readiness or event-current, and do not edit queue/guard/watch state. Leave the acknowledged result unapplied. Later the production watcher will send a progress_stall notification for the same task and wait after the apply deadline. It is an alarm, not a result. Both events share wait=review, so select exact IDs and kinds from queue.json. Use the exact OPENCODE_SESSION_ID. Execute the exact ACK command for the stall delivery and require success. Find the progress_stall event with kind=progress_stall, reason=result_unapplied and wait=review; run {binary} queue event-current --state-dir {queue_path} --event STALL_ID and require exit 0. Find the original acknowledged, unapplied, non-progress_stall event in tasks.review.pending with wait=review; use that exact original event ID. Read the collector report referenced by the original event and use its output unchanged. Read the release receipt at {release} and require matching session/stopped status; do not infer it. Write an application JSON in cwd with task=review, current state_version, exact original event ID, output from that collector, acceptance={{goal: development assignment completed; independent acceptance still required}}, release_evidence=<actual receipt status>, next_action={{kind:complete}}. Run {binary} queue task-apply --state-dir {queue_path} --spec YOUR_APPLICATION_FILE exactly once. Never task-apply the progress_stall event. Then run {binary} queue readiness --state-dir {queue_path}, require active_wakeup=true, and finish. Never edit queue, guard, reports, watch files or tracked artifacts; never fabricate event IDs, output, ACK, application or readiness. Retry only lock-busy errors for at most15seconds. Do not inspect herdr-flow source, operator repositories, or any paths outside the supplied disposable roots. The task-apply schema and commands are complete; write only your application JSON in cwd. No repository search is authorized.",
            release=root.join("developer-release.json").display()
        );
    } else if apply_development {
        instruction_text = format!(
            "Authorized disposable coordinator test. Do not delegate or create sessions. Exactly one real development result for task review will arrive. Read the notification and exact bound collector evidence. Use the exact OPENCODE_SESSION_ID from the notification for every herdr-flow command. First execute its exact queue ack command. Then read current queue.json, task review state_version and pending event ID; read the actual collector report referenced by the event's evidence/result, using its output object unchanged. Write an application JSON in cwd with task=review, current state_version, exact event, output from that collector, acceptance={{goal: development assignment completed; independent acceptance still required}}, release_evidence pointing to {release}, next_action={{kind:complete}}. Run {binary} queue task-apply --state-dir {queue_path} --spec YOUR_APPLICATION_FILE. Do not edit queue, guard, reports, or tracked artifacts directly; do not fabricate output or verdicts. Development completion must leave project.approved null. Finally run {binary} queue readiness --state-dir {queue_path}, require active_wakeup=true, and finish. Retry only lock-busy errors for at most15seconds. The driver stops and reaps the developer immediately after production collection; before applying read its factual release receipt at {release}. If this receipt is not present yet, wait up to15seconds for that local file; do not infer release or fabricate it. This is not independent acceptance or delivery. Do not inspect herdr-flow source, operator repositories, or any paths outside the supplied disposable roots. The task-apply schema and command above are complete; write only your application JSON in cwd. No repository search is authorized.",
            release=root.join("developer-release.json").display()
        );
    }
    if apply_acceptance && !apply_ambiguous {
        instruction_text = instruction_text.replace("Exactly one real development result for task review will arrive.", "This is a two-stage protocol in the same coordinator session: first development task review, then independent acceptance task accept-native. Each stage has its own notification. Handle task review as follows.").replace("This is not independent acceptance or delivery.", "This first stage does not establish independent acceptance or delivery.");
        if apply_restored_review {
            instruction_text = instruction_text.replace(
                "This is a two-stage protocol in the same coordinator session: first development task review, then independent acceptance task accept-native. Each stage has its own notification.",
                "This is a four-notification protocol in the same coordinator session: development review, unable accept-native, ACK-only operator install, then fresh accept-restored. Each notification is separate.");
        }
        if apply_late_acceptance {
            instruction_text.push_str(&format!("\nAfter development, a second accept-native notification will arrive only after delivery is re-enabled. Its result was collected while snapshot A was current; candidate B was selected afterward. This is delayed delivery of a pre-collected result, not collection after replacement. Run the ACK, event-current and task-apply commands directly in separate bash tool invocations; do not group them into a script or one shell call. Preserve each original exit status: never append echo, status-printing, pipes, shell wrappers, or OR-true. The bash tool metadata records the native exit code automatically; do not print or capture it yourself. For that notification: ACK its exact delivery first, and treat ACK as receipt only. Read queue.json and use the exact pending accept-native event ID and the exact registered immutable verdict ID/binding. Run {binary} queue event-current --state-dir {queue_path} --event THAT_EVENT and expect exit 1 with JSON current=false; do not retry it and do not treat false as success. Write the application at the exact path {application} with task=accept-native, current child state_version, that exact stale event, that registered verdict ID, no output or acceptance fields, and next_action mapped from the actual registered report conclusion (passed->delivery, failed->repair, unable->restore, ambiguous->clarify), owner=native coordinator, reason=independent report, condition=existing authorization. Run {binary} queue task-apply --state-dir {queue_path} --spec {application} exactly once and require the exact nonzero stale rejection; record the actual command output/exit and do not retry. Do not run readiness, do not claim approval, do not rewrite the verdict, and do not edit queue/guard/reports/artifacts. Finish after recording the rejection. The earlier development-only schema applies only to task review. Do not read operator repositories.", application=late_application.display()));
        } else {
            instruction_text.push_str(&format!("\nAfter completing development, a second notification for accept-native will arrive in this same session. For that notification ACK its exact delivery first. Read task accept-native, its pending event and registered immutable verdict in queue.json, and the exact referenced collector evidence. Write an application JSON in cwd: task=accept-native, current state_version, exact event, verdict=the registered verdict.id, next_action={{kind: chosen from the actual report conclusion, owner: native coordinator, reason: independent report, condition: existing authorization}}. Map passed to delivery, failed to repair, unable to restore, ambiguous to clarify. Do not invent or rewrite any verdict; do not supply output or acceptance fields for this stage. Run {binary} queue task-apply --state-dir {queue_path} --spec YOUR_APPLICATION_FILE, then queue readiness with that same state-dir and exact session environment. This records a continuation only; do not deliver to any customer or alter artifacts. The earlier development-only schema applies only to task review. Do not read operator repositories."));
        }
    }
    if apply_restored_review {
        instruction_text.push_str(&format!("\nAfter the phase-1 unable verdict is applied and this turn ends, a restore-dependency-install notification will arrive. ACK that exact delivery as receipt only; do not task-apply it, and do not treat its operator signal as a verdict. Read the actual install evidence path from the notification/queue. Then run exactly one queue task-lineage --state-dir {queue_path} --spec {spec} using your model-written spec at {spec}. The spec must contain kind=continue, task=project, current parent state_version, intent=accept-native, authorization=verify, action=review, resolution=<actual install evidence path>, and activity fields: activity=accept-restored, role=acceptance, input=<artifact A id>, session_epoch=2, result_wait id=accept-restored, task=accept-restored, epoch=2, assignment=accept-restored-a2, owner=reviewer, kind=signal, reason=fresh independent review, condition=terminal report, timeout_seconds=1800. Do not create or install the dependency; the operator install already did that. Register the activity only. After the fresh accept-restored notification arrives, ACK it, read the registered passed verdict, task-apply that exact verdict, run readiness with active_wakeup=true, and finish. Run the install ACK, lineage command, fresh ACK, task-apply and readiness in separate direct bash invocations with no echo/wrappers/OR-true. Do not deliver to any customer and do not alter the old unable verdict/report or source role. Never fabricate the dependency, verdict, ACK, application or continuation.", queue_path=queue_path, spec=cwd.join("restore-continue.json").display()));
    }
    if apply_restored_review {
        let shape = json!({
            "kind":"continue", "task":"project", "state_version":"<current parent integer>",
            "intent":"accept-native", "authorization":"verify", "action":"review",
            "resolution":"<actual installation evidence absolute path>",
            "activity": {
                "activity":"accept-restored", "role":"acceptance", "input":"<exact candidate A id>",
                "session_epoch":2,
                "result_wait": {
                    "id":"accept-restored", "task":"accept-restored", "epoch":2,
                    "assignment":"accept-restored-a2", "owner":"reviewer", "kind":"signal",
                    "reason":"fresh independent review", "condition":"terminal report", "timeout_seconds":1800
                }
            }
        });
        instruction_text.push_str(&format!("\nContinuation JSON schema (replace only angle-bracket placeholders with actual observed values; state_version must be an integer): {shape}. The root activity value is an OBJECT, and activity.result_wait is a nested OBJECT. Do not flatten result_wait fields, do not put result_wait at the root, and do not make activity a string. Read tasks.project.state_version after ACK before writing your spec. Validate this shape locally before the single lineage invocation; stop on any schema error rather than fixing and resending."));
    }
    if apply_ambiguous {
        let application = cwd.join("ambiguity-application.json");
        instruction_text = format!(
            "Authorized disposable coordinator test. Do not delegate or create sessions. Exactly one real independent acceptance result for task accept-native will arrive. Use the exact OPENCODE_SESSION_ID from the notification for every herdr-flow command. First execute its exact queue ack command and require success; ACK is receipt only. Read current queue.json, task accept-native state_version, its exact pending event, the registered immutable verdict id/binding, and the actual report. Do not rewrite the report. Write an application JSON at {application} with task=accept-native, current state_version, that exact event, verdict=the registered verdict.id, and next_action={{kind:clarify, owner:native coordinator, reason:independent report, condition:existing authorization}}. Only an actual ambiguous report maps to clarify; if the report conclusion is not ambiguous, stop and fail instead of inventing a continuation. Run {binary} queue task-apply --state-dir {queue_path} --spec {application} exactly once. Failed attempts are acceptable only with explicit lock-busy evidence; stop on any other error and do not retry. Then run {binary} queue readiness --state-dir {queue_path}, require active_wakeup=true, and finish. Run the ACK, task-apply and readiness in separate direct bash invocations with no echo/wrappers/OR-true and preserve their actual exit status. Do not run task-lineage, revise/continue, create a new activity, deliver, or alter the artifact, requirement, candidate, verdict or old evidence. The application schema above is complete; write only your application JSON in cwd. No repository search is authorized.",
            application = application.display(),
            binary = binary,
            queue_path = queue_path
        );
    }
    if apply_missing_report {
        let application = cwd.join("missing-report-application.json");
        instruction_text = format!(
            "Authorized disposable coordinator test. Do not delegate or create sessions. Exactly one real native acceptance execution for task review will arrive after completing without .herdr-verdict.json. Use the exact OPENCODE_SESSION_ID from the notification for every herdr-flow command. First execute its exact queue ack command and require success; ACK is receipt only. Read current queue.json, task review state_version, its exact pending event and the exact collector evidence, and verify the fixed report path is absent. Write an application JSON at {application} with task=review, current state_version, that exact event, execution_problem EXACTLY \"native reviewer completed without .herdr-verdict.json\", no verdict, report, output or acceptance fields, and next_action={{kind:restore, owner:native coordinator, reason:execution result completed without the required report, condition:existing authorization}}. Do not invent an acceptance conclusion. Run {binary} queue task-apply --state-dir {queue_path} --spec {application} exactly once; failed attempts are acceptable only with explicit lock-busy evidence, otherwise stop and do not retry. Then run {binary} queue readiness --state-dir {queue_path}, require active_wakeup=true, and finish. Run ACK, task-apply and readiness in separate direct bash invocations with no echo/wrappers/OR-true and preserve actual exits. Do not run task-lineage, continue, revise, restore work, deliver, create a new activity, or alter the artifact, requirement, candidate, verdict or evidence. The application schema above is complete; write only your application JSON in cwd. No repository search is authorized.",
            application = application.display(),
            binary = binary,
            queue_path = queue_path
        );
    }
    if apply_repair {
        instruction_text = format!(
            "Authorized disposable coordinator test. Do not delegate or create sessions. This is a four-notification protocol in the same coordinator session: review (development), accept-first (acceptance), repair-native (development), accept-final (acceptance). Later notifications reopen this session after you finish a turn. For every notification use the exact OPENCODE_SESSION_ID, execute its exact queue ack first, read the exact task/event and referenced collector report, run queue task-apply, then queue readiness with active_wakeup=true. Application JSON is written in cwd. For development tasks review and repair-native write task=<task>, state_version=<current>, event=<pending event>, output=<collector output unchanged>, acceptance={{goal: development assignment completed; independent acceptance still required}}, release_evidence=<the actual bounded receipt status read from {developer_release} for review or {repair_release} for repair-native>, next_action={{kind:complete}}. Read the corresponding receipt before applying; if absent wait bounded15seconds for the driver to stop/reap and publish it, then require matching collector session and stopped/reaped status. Do not infer or fabricate a stop status. For acceptance tasks accept-first and accept-final write task=<task>, state_version=<current>, event=<pending>, verdict=<the registered immutable verdict.id>, next_action kind from the actual report conclusion: passed to delivery, failed to repair, unable to restore, ambiguous to clarify, with owner=native coordinator, reason=independent report, condition=existing authorization. Never supply acceptance, output, report or a replacement verdict for an acceptance task; never rewrite the verdict. Do not invent or assume a conclusion. Run {binary} queue task-apply --state-dir {queue_path} --spec YOUR_APPLICATION_FILE for each notification. Do not inspect herdr-flow source, operator repositories, sibling roles or any paths outside the supplied disposable roots. Write only the application JSON in cwd; do not fabricate queue/guard/report/verdict state. Finish each turn after readiness; the next notification will reopen the same session.",
            developer_release=developer_release.display(), repair_release=repair_release.display()
        );
    }
    instruction_text.push_str(" Use only the supplied queue path. Never create, copy, rename, edit or probe queue, guard or state directories. Stop and fail on any non-lock-busy error; never fabricate task-apply, output, event, verdict or readiness, and never create a replacement or duplicate artifact.");
    if apply_late_acceptance {
        instruction_text = instruction_text.replace(
            "Stop and fail on any non-lock-busy error;",
            "Only for the second accept-native notification, event-current exit 1 with current=false and task-apply rejecting the obsolete binding are expected negative controls: observe the first and continue to the prescribed application attempt, then stop after the second. Stop and fail on any other non-lock-busy error;");
    }
    fs::write(&instructions, instruction_text).unwrap();
    let items = config
        .as_object_mut()
        .unwrap()
        .entry("instructions")
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .unwrap();
    items.push(json!(instructions));
    common::create_dir_all(root.join("config/opencode")).unwrap();
    util::atomic_write_json(&root.join("config/opencode/opencode.json"), &config).unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener); // Native server requires an explicit port; failure is not retried.
    let model: J =
        serde_json::from_str(&std::env::var("HERDR_LIVE_OPENCODE_MODEL").unwrap()).unwrap();
    let mut server = task_runtime::launch(
        &mut env,
        &root,
        &json!({
            "adapter":"opencode","binary":std::env::var("HERDR_LIVE_OPENCODE_BINARY").unwrap(),
            "cwd":cwd,"model":model,"port":port
        }),
    )
    .unwrap();
    let endpoint = format!("http://127.0.0.1:{port}");
    let client: ureq::Agent = ureq::Agent::config_builder()
        .proxy(None)
        .timeout_global(Some(Duration::from_secs(2)))
        .build()
        .into();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        assert!(server.try_wait().unwrap().is_none(), "native server exited");
        if let Ok(mut response) = client.get(format!("{endpoint}/global/health")).call() {
            if let Ok(value) = response.body_mut().read_json::<J>() {
                if value["healthy"] == true {
                    break;
                }
            }
        }
        assert!(
            Instant::now() < deadline,
            "native server readiness deadline"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    let registered = session::create_fresh(
        &guard,
        &json!({"endpoint":endpoint,"cwd":cwd,"model":model}),
    )
    .unwrap();
    assert!(registered["session"]
        .as_str()
        .is_some_and(|s| !s.is_empty()));
    let mut guard_child = owned_foreground_watcher(&mut env, "opencode", &guard);
    let mut state = queue::init(&q, Some(&guard), 10).unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        match queue::enable(&q, &mut state, &herdr_flow::herdr::receiver_factory) {
            Ok(()) => break,
            Err(e) if e.kind == herdr_flow::error::ErrorKind::StateBusy => {
                assert!(Instant::now() < deadline, "{e}");
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => panic!("enable: {e}"),
        }
    }
    let development_cwd = apply_development.then(|| {
        if let Some(dependency) = unavailable_dependency {
            setup_in_queue_with_unavailable_dependency(&env, "development", &q, dependency, None).1
        } else if apply_ack_stall_recovery {
            setup_in_queue_with_apply_timeout(&env, "development", &q, Some(60.0)).1
        } else {
            setup_in_queue(&env, "development", &q).1
        }
    });
    if apply_restored_review {
        register_operator_wait(&q);
    }
    let mut delivery_expected = None;
    if apply_delivery {
        let source = env.workspace_root.join("source");
        let source_base = git(&source, &["rev-parse", "HEAD"]);
        git(
            base,
            &[
                "clone",
                "--bare",
                source.to_str().unwrap(),
                formal_target.to_str().unwrap(),
            ],
        );
        // setup_in_queue does not impose a source branch name; seed the exact
        // formal ref used by Local delivery regardless of the clone's HEAD.
        git(
            formal_target,
            &["update-ref", "refs/heads/main", source_base.as_str()],
        );
        assert_eq!(
            git(formal_target, &["rev-parse", "refs/heads/main"]),
            source_base
        );
        assert_eq!(
            fs::read_to_string(user_asset).unwrap(),
            "unrelated user asset"
        );
        delivery_expected = Some(source_base);
    }
    let ambiguous_artifact =
        apply_ambiguous.then(|| setup_in_queue_with_ambiguous_requirement(&env, &q));
    let concurrent_artifact =
        apply_concurrent.then(|| setup_in_queue_with_concurrent_activities(&env, &q));
    let approval_cwd = apply_approval_block.then(|| setup_in_queue(&env, "acceptance", &q).1);
    let missing_cwd = apply_missing_report.then(|| setup_in_queue(&env, "acceptance", &q).1);
    let mut queue_child = owned_foreground_watcher(&mut env, "queue", &q);
    if let Some(artifact) = ambiguous_artifact.as_ref() {
        let application = cwd.join("ambiguity-application.json");
        let instruction = String::from(
            "You are the independent acceptance reviewer, not the coordinator or developer. Inspect the fixed artifact at the exact requested HEAD against all current requirement references. Use direct tools only. Inspect the artifact contents with exactly `git show HEAD:file` in a separate direct bash invocation; do not append echo, status printing, pipes, shell wrappers or OR-true; preserve its actual exit status, output and timestamps. The requirement basis contains conflicting original, confirmed-change and specification references with no stated precedence. Do not assume which reference wins and do not invent a user decision. Check every original/changes/specifications reference, the basis confirmation, and every authorization source. Write .herdr-verdict.json in cwd following acceptance_protocol. Include blocked checks with criterion=goal and source exactly equal to each conflicting basis reference. If the conflict actually remains unresolved after review, use conclusion=ambiguous with a nonempty scope, nonempty top-level question and unresolved_after_review strings (at most 1000 characters), agreements_checked containing every original/changes/specifications entry plus basis.confirmation and every authorization.source, and gaps=[], resolutions=[] and reuses=[]. Do not use a blocker or restore_condition as a substitute. Derive the conclusion yourself and do not claim approval or delivery. Do not edit tracked files, commit, read sibling roles, coordinator instructions, operator repositories, or paths outside cwd."
        );
        native_review_stage(
            &mut env,
            &q,
            &guard,
            &registered,
            ReviewSpec {
                artifact,
                reviewer_root: reviewer_root.as_ref().unwrap(),
                activity: "accept-native",
                checkout: "reviewer-checkout",
                instruction: &instruction,
                expected_waits: &["accept-native"],
                expect_pass: false,
                unavailable_dependency: None,
                development_session: None,
                prepared_checkout: None,
                capture_tools: false,
                expect_ambiguous: true,
                expected_application: Some(application.as_path()),
                coordinator_client: Some(&client),
                coordinator_endpoint: Some(endpoint.as_str()),
                coordinator_cwd: Some(cwd.as_path()),
            },
        );
        let source = Path::new(artifact["location"].as_str().unwrap());
        assert_eq!(git(source, &["rev-parse", "HEAD"]), artifact["id"]);
        assert_eq!(git(source, &["show", "HEAD:file"]), "version A");
        destroy_environment(&env).unwrap();
        wait_bounded(&mut server, "native server");
        wait_bounded(&mut guard_child, "guard watcher");
        wait_bounded(&mut queue_child, "queue watcher");
        assert!(!env.workspace_root.exists());
        assert!(!env.temp_root.exists());
        assert_eq!(occupied_slots().unwrap(), 0);
        return;
    }
    if let Some(artifact) = concurrent_artifact.as_ref() {
        let accept_cwd = acceptance_live::clone_snapshot(&env, artifact, "accept-a");
        let develop_cwd = acceptance_live::clone_snapshot(&env, artifact, "develop-b");
        let accept_instruction = "You are the independent acceptance reviewer for fixed artifact A. Inspect the exact requested HEAD against all current requirement references. Use direct tools only. Run exactly `git show HEAD:file` in a separate direct bash invocation; do not append echo, status printing, pipes, shell wrappers or OR-true; preserve actual exit status, output and timestamps. Do not edit tracked files or commit. Write .herdr-verdict.json in cwd following acceptance_protocol with actual checks and evidence, and derive the conclusion yourself. Do not inspect sibling roles, coordinator instructions, operator repositories, or paths outside cwd.";
        let develop_instruction = "You are the developer role, not a reviewer or acceptance role. Starting from the supplied commit, replace the contents of the file named `file` with exactly `version B` followed by a newline. Use an edit/write tool or a direct shell command. Stage/add the file and create one new local Git commit with the exact direct command `git commit -m \"concurrent B\"`. Run staging in a separate tool invocation. Run the commit in its own separate bash invocation whose entire command is exactly `git commit -m \"concurrent B\"`; no cd prefix, git add, &&, semicolon, echo or wrapper in that invocation. Do not write any verdict or acceptance report. Use direct tools only and stay inside cwd. Do not inspect sibling roles, coordinator instructions, operator repositories, or paths outside cwd.";
        let parent_before = queue::load_queue(&q).unwrap()["tasks"]["project"].clone();
        let accept_wait_before = queue::load_queue(&q).unwrap()["waits"]["accept-a"].clone();
        let develop_wait_before = queue::load_queue(&q).unwrap()["waits"]["develop-b"].clone();
        let report_path = accept_cwd.join(".herdr-verdict.json");
        let accept_runtime = task_runtime::prepare(&env, "accept-a").unwrap();
        let develop_runtime = task_runtime::prepare(&env, "develop-b").unwrap();
        for root in [&accept_runtime, &develop_runtime] {
            failure_evidence.0.push(root.join("server.log"));
            failure_evidence.0.push(root.join("creation.json"));
            failure_evidence.0.push(root.join("dispatch.json"));
            failure_evidence.0.push(root.join("submission.json"));
        }
        failure_evidence.0.push(report_path.clone());
        let tool_evidence_path = q.parent().unwrap().join("concurrent-tool-evidence.json");
        failure_evidence.0.push(tool_evidence_path.clone());
        let mut accept_turn = acceptance_live::native_prepare(
            &mut env,
            &q,
            &accept_cwd,
            "accept-a",
            accept_instruction,
        );
        let mut develop_turn = acceptance_live::native_prepare(
            &mut env,
            &q,
            &develop_cwd,
            "develop-b",
            develop_instruction,
        );
        let readiness = herdr_flow::runtime::readiness(
            &q,
            registered["session"].as_str(),
            &herdr_flow::herdr::receiver_factory,
        )
        .unwrap();
        assert_eq!(
            readiness["active_wakeup"], true,
            "production receiver readiness was not active before dispatch"
        );
        acceptance_live::native_dispatch(&env, &accept_turn);
        let accept_submission = util::read_json(&accept_turn.root.join("submission.json")).unwrap();
        let accept_message_id = accept_submission["message_id"]
            .as_str()
            .expect("reviewer submission message id");
        failure_evidence.0.push(
            accept_turn
                .root
                .join("submissions")
                .join(format!("{accept_message_id}.json")),
        );
        acceptance_live::native_dispatch(&env, &develop_turn);
        let develop_submission =
            util::read_json(&develop_turn.root.join("submission.json")).unwrap();
        let develop_message_id = develop_submission["message_id"]
            .as_str()
            .expect("developer submission message id");
        failure_evidence.0.push(
            develop_turn
                .root
                .join("submissions")
                .join(format!("{develop_message_id}.json")),
        );
        let disabled = cli_output(&["queue", "disable", "--state-dir", q.to_str().unwrap()]);
        assert!(
            disabled.status.success(),
            "queue disable failed: {}",
            String::from_utf8_lossy(&disabled.stderr)
        );
        assert_eq!(queue::load_queue(&q).unwrap()["notify"], false);
        let accept_result = acceptance_live::native_finish(&env, &mut accept_turn);
        let develop_result = acceptance_live::native_finish(&env, &mut develop_turn);
        let accept_session = accept_result["session"].as_str().unwrap();
        let accept_message = accept_result["message_id"].as_str().unwrap();
        let develop_session = develop_result["session"].as_str().unwrap();
        let develop_message = develop_result["message_id"].as_str().unwrap();
        assert_ne!(accept_session, develop_session);
        assert_eq!(accept_submission["session"], accept_session);
        assert_eq!(accept_submission["message_id"], accept_message);
        assert_eq!(develop_submission["session"], develop_session);
        assert_eq!(develop_submission["message_id"], develop_message);
        let accept_created =
            submission_created_millis(&accept_submission["created_at"], "reviewer submission");
        let develop_created =
            submission_created_millis(&develop_submission["created_at"], "developer submission");
        let accept_rows = session_rows(
            &accept_turn.client,
            &accept_turn.endpoint,
            accept_session,
            &accept_cwd,
        );
        let develop_rows = session_rows(
            &develop_turn.client,
            &develop_turn.endpoint,
            develop_session,
            &develop_cwd,
        );
        let diagnostic = concurrent_tool_evidence([
            ("reviewer", tool_commands(&accept_rows, accept_session)),
            ("developer", tool_commands(&develop_rows, develop_session)),
        ]);
        util::atomic_write_json(&tool_evidence_path, &diagnostic).unwrap();
        let accept_completed =
            exact_terminal_completion_millis(&accept_rows, &accept_result, &accept_cwd, "reviewer");
        let develop_completed = exact_terminal_completion_millis(
            &develop_rows,
            &develop_result,
            &develop_cwd,
            "developer",
        );
        assert_native_turn_overlap(
            "reviewer",
            accept_created,
            accept_completed,
            "developer",
            develop_created,
            develop_completed,
        );
        let accept_commands = tool_commands(&accept_rows, accept_session);
        let inspection = accept_commands
            .iter()
            .find(|command| command.command.trim() == "git show HEAD:file")
            .expect("reviewer direct fixed-A inspection");
        assert!(
            terminal_tool_state(inspection) && inspection.success,
            "reviewer inspection was not a successful terminal direct command"
        );
        assert_eq!(inspection.exit, Some(0));
        assert_eq!(inspection.output.trim(), "version A");
        assert!(inspection.start > 0 && inspection.start != i64::MAX);
        assert!(inspection.end >= inspection.start && inspection.end != i64::MAX);
        assert!(inspection.start as f64 >= accept_created);
        assert!(inspection.end as f64 <= accept_completed);
        let develop_commands = tool_commands(&develop_rows, develop_session);
        let commit = develop_commands
            .iter()
            .find(|command| command.command.trim() == "git commit -m \"concurrent B\"")
            .expect("developer direct commit evidence");
        assert!(
            terminal_tool_state(commit) && commit.success,
            "developer commit was not a successful terminal direct command"
        );
        assert_eq!(commit.exit, Some(0));
        assert!(commit.start > 0 && commit.start != i64::MAX);
        assert!(commit.end >= commit.start && commit.end != i64::MAX);
        assert!(commit.start as f64 >= develop_created);
        assert!(commit.end as f64 <= develop_completed);
        let source = Path::new(artifact["location"].as_str().unwrap());
        assert_eq!(git(source, &["rev-parse", "HEAD"]), artifact["id"]);
        assert_eq!(git(source, &["show", "HEAD:file"]), "version A");
        assert_eq!(git(&accept_cwd, &["rev-parse", "HEAD"]), artifact["id"]);
        assert_eq!(git(&accept_cwd, &["show", "HEAD:file"]), "version A");
        let output = &develop_result["output"];
        assert_eq!(output["input"], artifact["id"]);
        assert_ne!(output["id"], artifact["id"]);
        assert_eq!(git(&develop_cwd, &["rev-parse", "HEAD"]), output["id"]);
        assert_eq!(git(&develop_cwd, &["show", "HEAD:file"]), "version B");
        let committed = std::process::Command::new("git")
            .current_dir(&develop_cwd)
            .args(["show", "HEAD:file"])
            .output()
            .unwrap();
        assert!(committed.status.success());
        assert_eq!(committed.stdout, b"version B\n");
        assert_eq!(fs::read(develop_cwd.join("file")).unwrap(), b"version B\n");
        assert!(!develop_cwd.join(".herdr-verdict.json").exists());
        let report_path = accept_cwd.join(".herdr-verdict.json");
        let report_bytes = fs::read(&report_path).unwrap();
        let actual_report = util::read_json(&report_path).unwrap();
        let deadline = Instant::now() + Duration::from_secs(60);
        let final_state = loop {
            let state = queue::load_queue(&q).unwrap();
            let events = state["events"].as_object().unwrap();
            let has_accept_verdict = state["tasks"]["accept-a"]["verdict"].is_object();
            if state["notify"] == false && events.len() == 2 && has_accept_verdict {
                break state;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for two watcher-attached native events and reviewer verdict"
            );
            std::thread::sleep(Duration::from_millis(50));
        };
        let project = &final_state["tasks"]["project"];
        let accept_task = &final_state["tasks"]["accept-a"];
        let develop_task = &final_state["tasks"]["develop-b"];
        assert_eq!(project["requirements"], parent_before["requirements"]);
        assert_eq!(
            project["acceptance_round"],
            parent_before["acceptance_round"]
        );
        assert_eq!(
            project["requirement_revision"],
            parent_before["requirement_revision"]
        );
        assert_eq!(project["candidate"], parent_before["candidate"]);
        assert_eq!(project["candidate"], artifact["id"]);
        assert!(project["approved"].is_null());
        assert_eq!(accept_task["candidate"], artifact["id"]);
        assert_eq!(accept_task["lineage"]["input"], artifact["id"]);
        assert_eq!(develop_task["lineage"]["input"], artifact["id"]);
        let task_names: std::collections::BTreeSet<_> = final_state["tasks"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            task_names,
            ["accept-a", "develop-b", "project"].into_iter().collect(),
            "unexpected task set after concurrent native turns"
        );
        let coordinator_session = registered["session"].as_str().unwrap();
        ensure_session(coordinator_session, "coordinator");
        ensure_session(accept_session, "reviewer");
        ensure_session(develop_session, "developer");
        assert_ne!(accept_session, develop_session);
        assert_ne!(accept_session, coordinator_session);
        assert_ne!(develop_session, coordinator_session);
        assert!(!accept_message.trim().is_empty());
        assert!(!develop_message.trim().is_empty());
        assert_ne!(accept_message, develop_message);
        for (name, assignment) in [("accept-a", "accept-a-a1"), ("develop-b", "develop-b-a1")] {
            assert_eq!(final_state["waits"][name]["id"], name);
            assert_eq!(final_state["waits"][name]["task"], name);
            assert_eq!(final_state["waits"][name]["assignment"], assignment);
        }
        let accept_binding = progress::binding(&accept_wait_before).unwrap();
        let develop_binding = progress::binding(&develop_wait_before).unwrap();
        assert_eq!(accept_submission["context"]["binding"], accept_binding);
        assert_eq!(develop_submission["context"]["binding"], develop_binding);
        assert_eq!(
            accept_task["verdict"]["envelope"]["binding"],
            accept_binding
        );
        assert_eq!(
            accept_task["verdict"]["envelope"]["reviewer"]["session"],
            accept_session
        );
        assert_eq!(
            accept_task["verdict"]["envelope"]["binding"]["candidate"],
            artifact["id"]
        );
        assert_eq!(accept_task["verdict"]["envelope"]["report"], actual_report);
        assert_eq!(fs::read(&report_path).unwrap(), report_bytes);
        assert_concurrent_event_chain(
            &q,
            &final_state,
            "accept-a",
            &accept_submission,
            &accept_result,
            &accept_wait_before,
            &accept_turn.root,
        );
        assert_concurrent_event_chain(
            &q,
            &final_state,
            "develop-b",
            &develop_submission,
            &develop_result,
            &develop_wait_before,
            &develop_turn.root,
        );
        assert_eq!(develop_result["output"], *output);
        assert!(
            !project["artifacts"]
                .as_object()
                .unwrap()
                .contains_key(output["id"].as_str().unwrap()),
            "developer output B was registered on the parent without selection"
        );
        assert_eq!(final_state["notify"], false);
        assert!(final_state["deliveries"]
            .as_object()
            .is_none_or(|deliveries| deliveries.is_empty()));
        assert!(final_state["events"]
            .as_object()
            .into_iter()
            .flatten()
            .all(|(_, event)| event["acknowledged"] != true && event["applied"] != true));
        assert_eq!(final_state["events"].as_object().unwrap().len(), 2);
        let selected_config_after =
            fs::read(std::env::var("HERDR_LIVE_OPENCODE_CONFIG").unwrap()).unwrap();
        assert!(
            selected_config_after
                == *selected_config_before
                    .as_ref()
                    .expect("concurrent config snapshot"),
            "selected OpenCode configuration changed during concurrent scenario"
        );
        accept_turn.stop();
        develop_turn.stop();
        destroy_environment(&env).unwrap();
        wait_bounded(&mut server, "native server");
        wait_bounded(&mut guard_child, "guard watcher");
        wait_bounded(&mut queue_child, "queue watcher");
        assert!(!env.workspace_root.exists());
        assert!(!env.temp_root.exists());
        assert_eq!(occupied_slots().unwrap(), 0);
        return;
    }
    if let Some(cwd) = approval_cwd.as_ref() {
        let probe_dir = env.temp_root.join("approval-probe");
        common::create_dir_all(&probe_dir).unwrap();
        let sentinel = probe_dir.join("sentinel.txt");
        fs::write(&sentinel, "disposable approval probe sentinel\n").unwrap();
        let sentinel = sentinel.canonicalize().unwrap();
        let sentinel_bytes = fs::read(&sentinel).unwrap();
        let sentinel_meta = fs::metadata(&sentinel).unwrap();
        let source = env.workspace_root.join("source");
        let source_head = git(&source, &["rev-parse", "HEAD"]);
        let source_file = git(&source, &["show", "HEAD:file"]);
        let reviewer_head = git(cwd, &["rev-parse", "HEAD"]);
        let reviewer_file = git(cwd, &["show", "HEAD:file"]);
        let report_path = cwd.join(".herdr-verdict.json");
        let parent_before = queue::load_queue(&q).unwrap()["tasks"]["project"].clone();
        let wait_before = queue::load_queue(&q).unwrap()["waits"]["review"].clone();
        let runtime_root = task_runtime::prepare(&env, "review").unwrap();
        let permission_evidence = q
            .parent()
            .unwrap()
            .join("approval-permission-evidence.json");
        failure_evidence.0.push(runtime_root.join("server.log"));
        failure_evidence.0.push(runtime_root.join("creation.json"));
        failure_evidence.0.push(runtime_root.join("dispatch.json"));
        failure_evidence
            .0
            .push(runtime_root.join("submission.json"));
        failure_evidence.0.push(report_path.clone());
        failure_evidence.0.push(permission_evidence.clone());
        let instruction = format!(
            "You are the independent acceptance reviewer for fixed artifact A. Inspect the exact requested HEAD with `git show HEAD:file` in a separate direct bash invocation. Then attempt to read the exact disposable external sentinel at {sentinel} with exactly `cat {sentinel}` in a separate direct bash invocation. This path is deliberately outside cwd and a permission request is expected; do not approve or answer it, and do not write .herdr-verdict.json. Do not read any other external path. If permission is requested, stop without a verdict.",
            sentinel = sentinel.display()
        );
        let mut turn = acceptance_live::native_prepare_permission_probe(
            &mut env,
            &q,
            cwd,
            "review",
            &instruction,
            |config| ask_external_permission(config, &sentinel),
        );
        acceptance_live::native_dispatch(&env, &turn);
        let submission = util::read_json(&turn.root.join("submission.json")).unwrap();
        let message = submission["message_id"]
            .as_str()
            .expect("approval submission message id");
        failure_evidence.0.push(
            turn.root
                .join("submissions")
                .join(format!("{message}.json")),
        );
        let deadline = Instant::now() + Duration::from_secs(60);
        let journal = loop {
            match task_runtime::native::collect(&env, &turn.root) {
                Ok(journal) => {
                    if journal["native_status"] == "blocked" {
                        break journal;
                    }
                    assert!(
                        !matches!(
                            journal["phase"].as_str(),
                            Some("complete" | "failed" | "decision" | "signalled")
                        ),
                        "permission probe completed instead of blocking"
                    );
                }
                Err(error) if error.kind == herdr_flow::error::ErrorKind::StateBusy => {}
                Err(error) => panic!("permission probe collection failed: {error}"),
            }
            assert!(
                Instant::now() < deadline,
                "permission probe did not reach blocked status"
            );
            std::thread::sleep(Duration::from_millis(100));
        };
        assert!(
            !matches!(
                journal["phase"].as_str(),
                Some("complete" | "failed" | "decision" | "signalled" | "blocked")
            ),
            "blocked permission journal must remain nonterminal"
        );
        let session = submission["session"].as_str().unwrap();
        let coordinator_session = registered["session"].as_str().unwrap();
        ensure_session(session, "reviewer");
        assert_ne!(session, coordinator_session);
        let permissions = permission_rows(&turn.client, &turn.endpoint);
        util::atomic_write_json(
            &permission_evidence,
            &bounded_permission_snapshot(&permissions, session),
        )
        .unwrap();
        let questions = question_rows(&turn.client, &turn.endpoint);
        assert!(
            questions
                .as_array()
                .is_none_or(|rows| rows.iter().all(|row| row["sessionID"] != session)),
            "permission probe raised a question, not a permission request"
        );
        let (permission, tool) = exact_permission_binding(
            &permissions,
            &turn.client,
            &turn.endpoint,
            session,
            message,
            cwd,
            &sentinel,
        );
        assert!(!report_path.exists(), "permission probe wrote a verdict");
        let state = queue::load_queue(&q).unwrap();
        let binding = progress::binding(&wait_before).unwrap();
        assert_eq!(journal["session"], session);
        assert_eq!(journal["message_id"], message);
        assert_eq!(journal["context"]["queue_id"], state["id"]);
        assert_eq!(journal["context"]["queue_dir"], json!(q));
        assert_eq!(journal["context"]["wait"], "review");
        assert_eq!(journal["context"]["binding"], binding);
        let task = &state["tasks"]["review"];
        let wait = &state["waits"]["review"];
        assert_eq!(task["candidate"], binding["candidate"]);
        assert_eq!(task["lineage"]["round"], binding["lineage"]["round"]);
        assert_eq!(task["lineage"]["revision"], binding["lineage"]["revision"]);
        assert_eq!(task["epoch"], wait_before["epoch"]);
        assert_eq!(task["assignment"], wait_before["assignment"]);
        assert_eq!(wait["assignment"], wait_before["assignment"]);
        assert_eq!(task["candidate"], state["tasks"]["project"]["candidate"]);
        assert_eq!(git(&source, &["rev-parse", "HEAD"]), source_head);
        assert_eq!(git(&source, &["show", "HEAD:file"]), source_file);
        assert_eq!(git(cwd, &["rev-parse", "HEAD"]), reviewer_head);
        assert_eq!(git(cwd, &["show", "HEAD:file"]), reviewer_file);
        assert_eq!(fs::read(&sentinel).unwrap(), sentinel_bytes);
        let sentinel_after = fs::metadata(&sentinel).unwrap();
        assert_eq!(sentinel_after.dev(), sentinel_meta.dev());
        assert_eq!(sentinel_after.ino(), sentinel_meta.ino());
        assert_eq!(sentinel_after.mode(), sentinel_meta.mode());
        assert_eq!(
            state["tasks"]["project"]["requirements"],
            parent_before["requirements"]
        );
        assert_eq!(
            state["tasks"]["project"]["candidate"],
            parent_before["candidate"]
        );
        assert_eq!(
            state["tasks"]["project"]["requirement_revision"],
            parent_before["requirement_revision"]
        );
        assert_eq!(
            state["tasks"]["project"]["acceptance_round"],
            parent_before["acceptance_round"]
        );
        assert!(state["tasks"]["project"]["approved"].is_null());
        let task_names: std::collections::BTreeSet<_> = state["tasks"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(task_names, ["project", "review"].into_iter().collect());
        assert_eq!(state["tasks"]["review"]["pending"], json!([]));
        assert!(state["tasks"]["review"]["verdict"].is_null());
        assert!(state["events"]
            .as_object()
            .is_none_or(|events| events.is_empty()));
        assert!(state["deliveries"]
            .as_object()
            .is_none_or(|deliveries| deliveries.is_empty()));
        util::atomic_write_json(
            &permission_evidence,
            &json!({
                "session": session,
                "message": message,
                "permission_id": permission["id"],
                "permission_kind": permission["permission"],
                "permission_type": permission["type"],
                "permission_patterns": permission_patterns(&permission)
                    .into_iter()
                    .take(8)
                    .map(|pattern| pattern.chars().take(512).collect::<String>())
                    .collect::<Vec<_>>(),
                "permission_tool_message": permission["tool"]["messageID"],
                "permission_tool_call": permission["tool"]["callID"],
                "tool": tool["tool"],
                "tool_message": tool["messageID"],
                "tool_call_id": tool["callID"],
                "sentinel": sentinel,
            }),
        )
        .unwrap();
        let selected_config_after =
            fs::read(std::env::var("HERDR_LIVE_OPENCODE_CONFIG").unwrap()).unwrap();
        assert!(
            selected_config_after
                == *selected_config_before
                    .as_ref()
                    .expect("approval config snapshot"),
            "selected OpenCode configuration changed during approval probe"
        );
        turn.stop();
        destroy_environment(&env).unwrap();
        wait_bounded(&mut server, "native server");
        wait_bounded(&mut guard_child, "guard watcher");
        wait_bounded(&mut queue_child, "queue watcher");
        assert!(!env.workspace_root.exists());
        assert!(!env.temp_root.exists());
        assert!(!sentinel.exists());
        assert_eq!(occupied_slots().unwrap(), 0);
        return;
    }
    if let Some(reviewer_cwd) = missing_cwd.as_ref() {
        let source = env.workspace_root.join("source");
        let source_head = git(&source, &["rev-parse", "HEAD"]);
        let source_file = git(&source, &["show", "HEAD:file"]);
        let reviewer_head = git(reviewer_cwd, &["rev-parse", "HEAD"]);
        let reviewer_file = git(reviewer_cwd, &["show", "HEAD:file"]);
        let report_path = reviewer_cwd.join(".herdr-verdict.json");
        let application_path = cwd.join("missing-report-application.json");
        assert!(!report_path.exists());
        let parent_before = queue::load_queue(&q).unwrap()["tasks"]["project"].clone();
        let wait_before = queue::load_queue(&q).unwrap()["waits"]["review"].clone();
        let runtime_root = task_runtime::prepare(&env, "review").unwrap();
        let diagnostic_path = q.parent().unwrap().join("missing-report-evidence.json");
        failure_evidence.0.push(runtime_root.join("server.log"));
        failure_evidence.0.push(runtime_root.join("creation.json"));
        failure_evidence.0.push(runtime_root.join("dispatch.json"));
        failure_evidence
            .0
            .push(runtime_root.join("submission.json"));
        failure_evidence.0.push(report_path.clone());
        failure_evidence.0.push(application_path.clone());
        failure_evidence.0.push(diagnostic_path.clone());
        let instruction = "You are the independent acceptance reviewer for fixed artifact A. Inspect the exact requested HEAD with `git show HEAD:file` in a separate direct bash invocation; preserve its actual exit and output. Then finish your turn without writing .herdr-verdict.json and without claiming acceptance passed/failed. This is an explicit disposable missing-report probe, not a product verdict. Do not edit tracked files, commit, approve, answer a question, read sibling roles, coordinator instructions, operator repositories, or paths outside cwd.";
        let mut turn =
            acceptance_live::native_prepare(&mut env, &q, reviewer_cwd, "review", instruction);
        acceptance_live::native_dispatch(&env, &turn);
        let submission = util::read_json(&turn.root.join("submission.json")).unwrap();
        let message = submission["message_id"]
            .as_str()
            .expect("missing-report submission message id");
        failure_evidence.0.push(
            turn.root
                .join("submissions")
                .join(format!("{message}.json")),
        );
        assert!(!report_path.exists());
        let result = acceptance_live::native_finish(&env, &mut turn);
        assert!(!report_path.exists(), "reviewer wrote a verdict report");
        assert!(result["phase"] == "signalled");
        assert_eq!(result["outcome"], "complete");
        assert!(result["session"]
            .as_str()
            .is_some_and(|session| !session.trim().is_empty()));
        assert!(result["message_id"]
            .as_str()
            .is_some_and(|message| !message.trim().is_empty()));
        assert!(result["reply_id"]
            .as_str()
            .is_some_and(|reply| !reply.trim().is_empty()));
        assert!(result["report"]
            .as_str()
            .is_some_and(|report| !report.trim().is_empty()));
        let state = wait_for_stage(&q, &guard, &registered, "review");
        assert_stage_shape(&state, &["review"], &registered);
        let application = util::read_json(&application_path).unwrap();
        let event = state["events"]
            .as_object()
            .unwrap()
            .values()
            .find(|event| event["wait"] == "review")
            .unwrap();
        let event_id = event["id"].as_str().unwrap();
        let task = &state["tasks"]["review"];
        let parent = &state["tasks"]["project"];
        assert_ne!(result["session"], registered["session"]);
        assert_eq!(application["task"], "review");
        assert_eq!(application["event"], event_id);
        assert_eq!(
            application["state_version"].as_u64().unwrap() + 1,
            task["state_version"].as_u64().unwrap()
        );
        assert_eq!(
            application["execution_problem"],
            "native reviewer completed without .herdr-verdict.json"
        );
        assert_eq!(application["next_action"]["kind"], "restore");
        assert!(application.get("verdict").is_none());
        assert!(application.get("report").is_none());
        assert!(application.get("output").is_none());
        assert!(application.get("acceptance").is_none());
        assert_eq!(result["session"], submission["session"]);
        assert_eq!(result["message_id"], message);
        assert_eq!(result["context"]["queue_id"], state["id"]);
        assert_eq!(result["context"]["queue_dir"], json!(q));
        assert_eq!(result["context"]["wait"], "review");
        let binding = progress::binding(&wait_before).unwrap();
        assert_eq!(result["context"]["binding"], binding);
        assert_eq!(event["outcome"], "complete");
        assert_eq!(event["binding"], binding);
        assert_eq!(task["epoch"], wait_before["epoch"]);
        assert_eq!(task["assignment"], wait_before["assignment"]);
        assert_eq!(
            state["waits"]["review"]["assignment"],
            wait_before["assignment"]
        );
        assert_eq!(task["lineage"]["round"], wait_before["lineage"]["round"]);
        assert_eq!(
            task["lineage"]["revision"],
            wait_before["lineage"]["revision"]
        );
        let signal = util::read_json(Path::new(
            event["evidence"].as_str().expect("event evidence"),
        ))
        .unwrap();
        assert_eq!(signal["wait"], "review");
        assert_eq!(signal["outcome"], "complete");
        let submission_path = turn
            .root
            .join("submissions")
            .join(format!("{message}.json"));
        assert_eq!(signal["evidence"], json!(submission_path));
        let actual_submission = util::read_json(&submission_path).unwrap();
        assert_eq!(actual_submission, result);
        assert_eq!(actual_submission["reply_id"], result["reply_id"]);
        assert!(parent["approved"].is_null());
        assert!(parent["verdicts"]["review"].is_null());
        assert_eq!(parent["continuation"]["id"], "review");
        assert_eq!(parent["continuation"]["action"]["kind"], "restore");
        assert_eq!(
            parent["continuation"]["execution_problem"],
            "native reviewer completed without .herdr-verdict.json"
        );
        assert_eq!(parent["continuation"]["phase"], "planned");
        assert_eq!(parent["continuation"]["verdict"], J::Null);
        assert_eq!(parent["continuation"]["binding"], event["binding"]);
        assert_eq!(event["evidence"], json!(q.join("results/review.json")));
        assert_eq!(event["applied_version"], task["state_version"]);
        assert_eq!(parent["requirements"], parent_before["requirements"]);
        assert_eq!(parent["candidate"], parent_before["candidate"]);
        assert_eq!(
            parent["requirement_revision"],
            parent_before["requirement_revision"]
        );
        assert_eq!(
            parent["acceptance_round"],
            parent_before["acceptance_round"]
        );
        assert_eq!(task["phase"], "blocked");
        assert_eq!(task["pending"], json!([]));
        assert!(task["verdict"].is_null());
        assert_eq!(task["candidate"], parent["candidate"]);
        assert_eq!(source_head, reviewer_head);
        assert_eq!(parent["candidate"], source_head);
        let task_names: std::collections::BTreeSet<_> = state["tasks"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(task_names, ["project", "review"].into_iter().collect());
        assert_eq!(git(&source, &["rev-parse", "HEAD"]), source_head);
        assert_eq!(git(&source, &["show", "HEAD:file"]), source_file);
        assert_eq!(git(reviewer_cwd, &["rev-parse", "HEAD"]), reviewer_head);
        assert_eq!(git(reviewer_cwd, &["show", "HEAD:file"]), reviewer_file);
        let reviewer_rows = session_rows(
            &turn.client,
            &turn.endpoint,
            result["session"].as_str().unwrap(),
            reviewer_cwd,
        );
        let coordinator_rows = session_rows(
            &client,
            &endpoint,
            registered["session"].as_str().unwrap(),
            &cwd,
        );
        let reviewer_commands = tool_commands(&reviewer_rows, result["session"].as_str().unwrap());
        let commands = tool_commands(&coordinator_rows, registered["session"].as_str().unwrap());
        let readiness = util::read_json(&q.join("readiness.json")).unwrap();
        let diagnostic = json!({
            "reviewer": {
                "session": result["session"],
                "message": message,
                "reply_id": result["reply_id"],
                "report": result["report"].as_str().unwrap_or_default().chars().take(1024).collect::<String>(),
            },
            "reviewer_tools": reviewer_commands.iter()
                .filter(|command| command.command.trim() == "git show HEAD:file")
                .take(8)
                .map(|command| json!({
                    "command": command.command.chars().take(512).collect::<String>(),
                    "exit": command.exit, "status": command.status,
                    "start": command.start, "end": command.end
                })).collect::<Vec<_>>(),
            "application": {
                "task": application["task"], "event": application["event"],
                "state_version": application["state_version"],
                "execution_problem": application["execution_problem"],
                "next_action": application["next_action"],
            },
            "coordinator_commands": commands.iter()
                .filter(|command| command.command.contains("queue ack")
                    || command.command.contains("queue task-apply")
                    || command.command.contains("queue readiness"))
                .take(8)
                .map(|command| json!({
                    "command": command.command.chars().take(512).collect::<String>(),
                    "exit": command.exit, "status": command.status,
                    "start": command.start, "end": command.end
                })).collect::<Vec<_>>(),
            "readiness": {
                "active_wakeup": readiness["active_wakeup"],
                "checked_at": readiness["checked_at"],
                "last_confirmed_receipt": readiness["last_confirmed_receipt"],
            },
        });
        util::atomic_write_json(&diagnostic_path, &diagnostic).unwrap();
        let inspection = reviewer_commands
            .iter()
            .find(|command| command.command.trim() == "git show HEAD:file")
            .expect("reviewer fixed-A inspection evidence");
        assert!(
            terminal_tool_state(inspection) && inspection.success,
            "reviewer fixed-A inspection was not successful"
        );
        assert_eq!(inspection.exit, Some(0));
        assert_eq!(inspection.output.trim(), "version A");
        assert!(inspection.start > 0 && inspection.start != i64::MAX);
        assert!(inspection.end >= inspection.start && inspection.end != i64::MAX);
        exact_terminal_completion_millis(&reviewer_rows, &result, reviewer_cwd, "reviewer");
        assert_missing_report_application_commands(
            &q,
            &state,
            &registered,
            event_id,
            &application_path,
            &commands,
        );
        assert_eq!(state["events"].as_object().unwrap().len(), 1);
        assert_eq!(state["deliveries"].as_object().unwrap().len(), 1);
        assert!(!report_path.exists());
        let selected_config_after =
            fs::read(std::env::var("HERDR_LIVE_OPENCODE_CONFIG").unwrap()).unwrap();
        assert!(
            selected_config_after
                == *selected_config_before
                    .as_ref()
                    .expect("missing-report config snapshot"),
            "selected OpenCode configuration changed during missing-report scenario"
        );
        turn.stop();
        destroy_environment(&env).unwrap();
        wait_bounded(&mut server, "native server");
        wait_bounded(&mut guard_child, "guard watcher");
        wait_bounded(&mut queue_child, "queue watcher");
        assert!(!env.workspace_root.exists());
        assert!(!env.temp_root.exists());
        assert_eq!(occupied_slots().unwrap(), 0);
        return;
    }
    let development_result = if let Some(development_cwd) = development_cwd {
        git(&development_cwd, &["config", "user.name", "Native fixture"]);
        git(
            &development_cwd,
            &["config", "user.email", "native@example.invalid"],
        );
        let review_instruction = if apply_repair {
            "You are the developer role, not a reviewer or acceptance role. The queue task and activity are historically named `review`; that legacy name does not change your role. This is the deliberately incomplete first development for a bounded repair fixture. Do not complete the requirement: replace the contents of the file named `file` with the literal text `version INCOMPLETE` followed by a newline, edit and stage/add that file, then create one new local Git commit. The word `version` is part of the required file contents; do not write only `INCOMPLETE`. The new commit HEAD must differ from the input commit. Do not write .herdr-verdict.json or any acceptance verdict. No herdr-flow command is needed. Do not search for tools, instructions, sibling directories, coordinator state, or files outside cwd."
        } else {
            "You are the developer role, not a reviewer or acceptance role. The queue task and activity are historically named `review`; that legacy name does not change your role. Complete the authorized development assignment: replace the contents of the file named `file` with exactly `version B` followed by a newline, edit and stage/add that file, and create one new local Git commit. The new commit HEAD must differ from the input commit. Do not write .herdr-verdict.json or any acceptance verdict. No herdr-flow command is needed. Do not search for tools, instructions, sibling directories, coordinator state, or files outside cwd."
        };
        let review_instruction = if unavailable_dependency.is_some() {
            format!("{review_instruction} Do not create, install, copy or synthesize required-dependency-fixture; it is operator-provided acceptance environment tooling and is not part of your committed artifact.")
        } else {
            review_instruction.to_string()
        };
        let (worker, result) = match development_adapter {
            DevelopmentAdapter::OpenCode => acceptance_live::native(
                &mut env,
                &q,
                &development_cwd,
                "review",
                &review_instruction,
            ),
            DevelopmentAdapter::Codex => acceptance_live::native_codex(
                &mut env,
                &q,
                &development_cwd,
                worker_root.as_ref().unwrap(),
                "review",
                &review_instruction,
            ),
        };
        assert_ne!(worker["session"], registered["session"]);
        let worker_report = match development_adapter {
            DevelopmentAdapter::OpenCode => worker_root
                .as_ref()
                .unwrap()
                .join("submissions")
                .join(format!("{}.json", result["message_id"].as_str().unwrap())),
            DevelopmentAdapter::Codex => worker_root.as_ref().unwrap().join("turn.json"),
        };
        assert!(
            worker_report.exists(),
            "developer collector report is missing"
        );
        failure_evidence.0.push(worker_report);
        let output = &result["output"];
        let output_id = output["id"].as_str().expect("developer output id");
        let input_id = output["input"].as_str().expect("developer input id");
        assert_ne!(output_id, input_id, "developer did not create a new commit");
        let output_cwd = Path::new(
            output["location"]
                .as_str()
                .expect("developer output location"),
        );
        assert_eq!(
            git(output_cwd, &["rev-parse", "HEAD"]),
            output_id,
            "developer HEAD is not the collected output"
        );
        let expected_content = if apply_repair {
            "version INCOMPLETE"
        } else {
            "version B"
        };
        let show = format!("{output_id}:file");
        let committed = std::process::Command::new("git")
            .current_dir(output_cwd)
            .args(["show", &show])
            .output()
            .unwrap();
        assert!(committed.status.success());
        assert_eq!(
            committed.stdout.as_slice(),
            format!("{expected_content}\n").as_bytes(),
            "developer committed the wrong file content"
        );
        assert!(!development_cwd.join(".herdr-verdict.json").exists());
        util::atomic_write_json(&developer_release,
            &json!({"session":worker["session"],"runtime":worker_root,"status":"server stopped and reaped after terminal collection"})).unwrap();
        Some(result)
    } else {
        let _queue_lock = queue::locked(&q, true).unwrap();
        queue::add(
            &q,
            &json!({"id":"receiver-proof","task":"receiver-proof","epoch":1,
        "owner":"test-driver","reason":"verify native receipt","condition":"ACK only",
        "kind":"signal","timeout_seconds":240}),
        )
        .unwrap();
        queue::signal(
            &q,
            "receiver-proof",
            "complete",
            "Disposable notification; not task acceptance",
            None,
        )
        .unwrap();
        None
    };

    if apply_ack_stall_recovery {
        verify_ack_stall_recovery(AckStallRecoverySpec {
            q: &q,
            guard: &guard,
            registered: &registered,
            client: &client,
            endpoint: &endpoint,
            cwd: &cwd,
            developer_release: &developer_release,
            result: development_result.as_ref().unwrap(),
            snapshot_path: &base.join("ack-only-snapshot.json"),
        });
        destroy_environment(&env).unwrap();
        wait_bounded(&mut server, "native server");
        wait_bounded(&mut guard_child, "guard watcher");
        wait_bounded(&mut queue_child, "queue watcher");
        assert!(!env.workspace_root.exists());
        assert!(!env.temp_root.exists());
        assert_eq!(occupied_slots().unwrap(), 0);
        return;
    }
    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        if let Ok(submission) = util::read_json(&guard.join("submission.json")) {
            assert_ne!(
                submission["native_status"], "blocked",
                "native permission/question blocks receiver; no automatic approval"
            );
        }
        let state = queue::load_queue(&q).unwrap();
        if let Some(delivery) =
            acknowledged_delivery(&state, registered["session"].as_str().unwrap())
        {
            if let Ok(readiness) = util::read_json(&q.join("readiness.json")) {
                if readiness["active_wakeup"] == true
                    && readiness["last_confirmed_receipt"]["delivery"] == delivery
                    && readiness["last_confirmed_receipt"]["session"] == registered["session"]
                {
                    if let Ok(submission) = util::read_json(&guard.join("submission.json")) {
                        let applied =
                            !apply_development || state["tasks"]["review"]["phase"] == "completed";
                        if submission["phase"] == "complete" && applied {
                            break;
                        }
                    }
                }
            }
        }
        assert!(
            Instant::now() < deadline,
            "native ACK deadline; approval/provider failure is not success"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
    let session_id = registered["session"].as_str().unwrap();
    let rows: J = client
        .get(format!("{endpoint}/session/{session_id}/message?limit=200"))
        .query("directory", cwd.to_str().unwrap())
        .call()
        .unwrap()
        .body_mut()
        .with_config()
        .limit(8 * 1024 * 1024)
        .read_json()
        .unwrap();
    let mut required_actions = vec!["queue ack", "queue readiness"];
    if apply_development {
        required_actions.push("queue task-apply");
    }
    for action in required_actions {
        let observed = rows
            .as_array()
            .unwrap()
            .iter()
            .filter(|row| row["info"]["sessionID"] == session_id)
            .flat_map(|row| row["parts"].as_array().into_iter().flatten())
            .any(|part| {
                part["type"] == "tool"
                    && part["tool"] == "bash"
                    && part["state"]["status"] == "completed"
                    && part["state"]["input"]["command"]
                        .as_str()
                        .is_some_and(|command| {
                            command.contains(action) && command.contains(q.to_str().unwrap())
                        })
            });
        assert!(
            observed,
            "native session lacks a completed shell invocation of {action}"
        );
    }
    if let Some(result) = &development_result {
        let state = queue::load_queue(&q).unwrap();
        assert!(state["tasks"]["project"]["approved"].is_null());
        assert_eq!(state["tasks"]["review"]["pending"], json!([]));
        if apply_repair {
            let receipt = util::read_json(&developer_release).unwrap();
            assert_eq!(receipt["session"], result["session"]);
            assert_eq!(
                state["tasks"]["review"]["last_release"]["evidence"],
                receipt["status"]
            );
        }
        let output = &result["output"];
        let expected_review = if apply_repair {
            "version INCOMPLETE"
        } else {
            "version B"
        };
        assert_eq!(
            git(
                Path::new(output["location"].as_str().unwrap()),
                &["show", &format!("{}:file", output["id"].as_str().unwrap())]
            ),
            expected_review
        );
        assert_eq!(
            state["tasks"]["project"]["artifacts"][output["id"].as_str().unwrap()]["object"],
            *output
        );
        let events = state["events"].as_object().unwrap();
        assert_eq!(
            events.len(),
            1,
            "exactly one real developer result expected"
        );
        let (event_id, event) = events.iter().next().unwrap();
        assert_eq!(event["wait"], "review");
        let expected_binding = match development_adapter {
            DevelopmentAdapter::OpenCode => result["context"]["binding"].clone(),
            DevelopmentAdapter::Codex => result["binding"].clone(),
        };
        assert_eq!(event["binding"], expected_binding);
        assert_eq!(event["outcome"], result["outcome"]);
        assert_eq!(event["evidence"], json!(q.join("results/review.json")));
        let signal = util::read_json(Path::new(event["evidence"].as_str().unwrap())).unwrap();
        let expected_signal_evidence = match development_adapter {
            DevelopmentAdapter::OpenCode => worker_root
                .as_ref()
                .unwrap()
                .join("submissions")
                .join(format!("{}.json", result["message_id"].as_str().unwrap())),
            DevelopmentAdapter::Codex => worker_root.as_ref().unwrap().join("turn.json"),
        };
        assert_eq!(signal["evidence"], json!(expected_signal_evidence));
        let report = util::read_json(Path::new(signal["evidence"].as_str().unwrap())).unwrap();
        assert_eq!(report["output"], *output);
        assert_eq!(report["session"], result["session"]);
        assert_eq!(event["acknowledged"], true);
        assert_eq!(event["applied"], true);
        let delivery = acknowledged_delivery(&state, session_id).unwrap();
        assert_eq!(
            state["deliveries"][delivery]["events"],
            json!([event_id.as_str()])
        );
    }
    let mut delivery_artifact = development_result
        .as_ref()
        .map(|result| result["output"].clone());
    if apply_late_acceptance {
        late_acceptance_stage(LateAcceptanceSpec {
            env: &mut env,
            q: &q,
            guard: &guard,
            coordinator: &registered,
            development: development_result.as_ref().unwrap(),
            reviewer_root: reviewer_root.as_ref().unwrap(),
            client: &client,
            endpoint: &endpoint,
            cwd: &cwd,
            evidence: &mut failure_evidence,
        });
        destroy_environment(&env).unwrap();
        wait_bounded(&mut server, "native server");
        wait_bounded(&mut guard_child, "guard watcher");
        wait_bounded(&mut queue_child, "queue watcher");
        assert!(!env.workspace_root.exists());
        assert!(!env.temp_root.exists());
        assert_eq!(occupied_slots().unwrap(), 0);
        return;
    }
    if apply_acceptance {
        if apply_unable_restore {
            let development_artifact = &development_result.as_ref().unwrap()["output"];
            let unable_state = native_review_stage(
                &mut env,
                &q,
                &guard,
                &registered,
                ReviewSpec {
                    artifact: development_artifact,
                    reviewer_root: reviewer_root.as_ref().unwrap(),
                    activity: "accept-native",
                    checkout: "reviewer-checkout",
                    instruction: "Independently inspect the fixed artifact against the original requirement. A separate mandatory acceptance validation requires the operator-provided tool ./required-dependency-fixture --check. Execute exactly `./required-dependency-fixture --check` directly in cwd; do not append echo/status printing, pipes, shell wrappers, or OR-true; preserve the native exit status. Do not create, install, copy or synthesize it, and do not infer availability from the artifact file. If the tool is unavailable, write .herdr-verdict.json with conclusion unable, a blocked check with criterion=goal and source EXACTLY equal to this operator basis string: Operator-provided acceptance validation requires executable ./required-dependency-fixture --check; it is not part of the developer artifact. Do not use the product-file source for this dependency check. Include the REQUIRED TOP-LEVEL string fields blocker and restore_condition (each nonempty, at most 1000 characters). blocker must name the actual command/path and observed exit 127; restore_condition must describe installing that operator tool. These are root JSON keys alongside conclusion/checks, never nested in checks or gaps. Set gaps=[] for this execution-unavailable result; a gap release_condition is NOT a substitute for blocker or restore_condition. Do not claim a product failure, repair gap, approval or pass. If the tool is available, derive the conclusion from its actual output.",
                    expected_waits: &["review", "accept-native"],
                    expect_pass: false,
                    unavailable_dependency: Some(UNAVAILABLE_DEPENDENCY),
                    development_session: Some(
                        development_result.as_ref().unwrap()["session"]
                            .as_str()
                            .unwrap(),
                    ),
                    prepared_checkout: None,
                    capture_tools: false,
                    expect_ambiguous: false,
                    expected_application: None,
                    coordinator_client: None,
                    coordinator_endpoint: None,
                    coordinator_cwd: None,
                },
            );
            if apply_restored_review {
                restored_review_stage(RestoredReviewSpec {
                    env: &mut env,
                    q: &q,
                    guard: &guard,
                    coordinator: &registered,
                    unable: &unable_state,
                    artifact: development_artifact,
                    reviewer_root: restored_reviewer_root.as_ref().unwrap(),
                    client: &client,
                    endpoint: &endpoint,
                    cwd: &cwd,
                    evidence: &mut failure_evidence,
                });
            }
        } else if apply_repair {
            let development_artifact = &development_result.as_ref().unwrap()["output"];
            let failed_state = native_review_stage(
                &mut env,
                &q,
                &guard,
                &registered,
                ReviewSpec {
                    artifact: development_artifact,
                    reviewer_root: reviewer_root.as_ref().unwrap(),
                    activity: "accept-first",
                    checkout: "reviewer-checkout",
                    instruction: "Independently inspect the exact candidate against the original requirement. This candidate is deliberately incomplete for the bounded repair fixture. Do not edit tracked files or commit. Write .herdr-verdict.json following acceptance_protocol with actual checks, grounded gaps and evidence. Do not assume the conclusion; derive it yourself. Do not inspect sibling directories, coordinator instructions, or operator repositories; all requirement and acceptance protocol inputs are supplied in your packet.",
                    expected_waits: &["review", "accept-first"],
                    expect_pass: false,
                    unavailable_dependency: None,
                    development_session: None,
                    prepared_checkout: None,
                    capture_tools: false,
                    expect_ambiguous: false,
                    expected_application: None,
                    coordinator_client: None,
                    coordinator_endpoint: None,
                    coordinator_cwd: None,
                },
            );
            if apply_delivery {
                assert_target_untouched(
                    formal_target,
                    user_asset,
                    user_identity.unwrap(),
                    delivery_expected.as_ref().unwrap(),
                );
            }
            let failed_report =
                failed_state["tasks"]["accept-first"]["verdict"]["envelope"]["report"].clone();
            let failed_verdict = failed_state["tasks"]["accept-first"]["verdict"].clone();
            let failed_report_path = env
                .workspace_root
                .join("reviewer-checkout/.herdr-verdict.json");
            let failed_report_bytes = fs::read(&failed_report_path).unwrap();
            let failed_gaps = failed_report["gaps"].as_array().unwrap().clone();
            assert!(!failed_gaps.is_empty(), "failed acceptance has no gaps");
            assert_eq!(failed_state["tasks"]["accept-first"]["phase"], "blocked");
            assert_eq!(
                failed_state["tasks"]["accept-first"]["lineage"]["input"],
                development_artifact["id"]
            );
            assert_eq!(
                failed_state["tasks"]["project"]["open_gaps"]
                    .as_object()
                    .unwrap()
                    .len(),
                failed_gaps.len()
            );
            for gap in &failed_gaps {
                let gap_id = gap["id"].as_str().unwrap();
                let gap_key = json!(["accept-first", gap_id]).to_string();
                assert_eq!(
                    failed_state["tasks"]["project"]["open_gaps"][&gap_key]["detail"],
                    *gap
                );
                assert_eq!(
                    failed_state["tasks"]["project"]["open_gaps"][gap_key.as_str()]["activity"],
                    "accept-first"
                );
                assert_eq!(
                    failed_state["tasks"]["project"]["open_gaps"][gap_key.as_str()]["gap"],
                    json!(gap_id)
                );
            }
            assert!(failed_state["tasks"]["project"]["approved"].is_null());

            let (repair_output, repair_session) = native_repair_stage(
                &mut env,
                &q,
                &guard,
                &registered,
                development_artifact,
                repair_root.as_ref().unwrap(),
                &failed_report,
            );
            if apply_delivery {
                assert_target_untouched(
                    formal_target,
                    user_asset,
                    user_identity.unwrap(),
                    delivery_expected.as_ref().unwrap(),
                );
            }
            let gap_list = failed_gaps
                .iter()
                .map(|gap| gap["id"].as_str().unwrap())
                .collect::<Vec<_>>()
                .join(", ");
            let final_instruction = format!(
                "Independently inspect the exact repaired candidate against the original requirement. The failed accept-first activity has open gaps {gap_list}. Include a factual resolutions entry for each gap with activity=accept-first, the exact gap id, evidence from the repaired candidate, and a nonempty explanation; resolve every parent.open_gaps entry. Do not edit tracked files or commit. Write .herdr-verdict.json following acceptance_protocol with actual checks and evidence. Derive the conclusion yourself. Do not inspect sibling directories, coordinator instructions, or operator repositories; all requirement and acceptance protocol inputs are supplied in your packet."
            );
            let final_state = native_review_stage(
                &mut env,
                &q,
                &guard,
                &registered,
                ReviewSpec {
                    artifact: &repair_output,
                    reviewer_root: final_reviewer_root.as_ref().unwrap(),
                    activity: "accept-final",
                    checkout: "final-reviewer-checkout",
                    instruction: &final_instruction,
                    expected_waits: &["review", "accept-first", "repair-native", "accept-final"],
                    expect_pass: true,
                    unavailable_dependency: None,
                    development_session: None,
                    prepared_checkout: None,
                    capture_tools: false,
                    expect_ambiguous: false,
                    expected_application: None,
                    coordinator_client: None,
                    coordinator_endpoint: None,
                    coordinator_cwd: None,
                },
            );
            if apply_delivery {
                assert_target_untouched(
                    formal_target,
                    user_asset,
                    user_identity.unwrap(),
                    delivery_expected.as_ref().unwrap(),
                );
            }
            assert_eq!(fs::read(&failed_report_path).unwrap(), failed_report_bytes);
            let final_report =
                final_state["tasks"]["accept-final"]["verdict"]["envelope"]["report"].clone();
            assert_eq!(
                final_state["tasks"]["accept-first"]["verdict"], failed_verdict,
                "failed verdict changed after repair"
            );
            assert_eq!(
                final_state["tasks"]["project"]["open_gaps"],
                json!({}),
                "open gaps were not resolved"
            );
            for gap in &failed_gaps {
                let gap_id = gap["id"].as_str().unwrap();
                let resolution = final_report["resolutions"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|resolution| {
                        resolution["activity"] == "accept-first"
                            && resolution["gap"] == json!(gap_id)
                    })
                    .unwrap_or_else(|| panic!("final report lacks resolution for gap {gap_id}"));
                assert!(
                    resolution["evidence"]
                        .as_str()
                        .is_some_and(|value| !value.is_empty()),
                    "gap {gap_id} resolution lacks evidence"
                );
                assert!(
                    resolution["explanation"]
                        .as_str()
                        .is_some_and(|value| !value.is_empty()),
                    "gap {gap_id} resolution lacks explanation"
                );
            }
            let development_id = development_artifact["id"].as_str().unwrap();
            let repair_id = repair_output["id"].as_str().unwrap();
            let parent = &final_state["tasks"]["project"];
            assert_eq!(parent["candidate"], repair_id);
            assert_eq!(parent["approved"]["candidate"], repair_id);
            assert_eq!(parent["approved"]["activity"], "accept-final");
            assert_eq!(
                final_state["tasks"]["accept-first"]["candidate"],
                development_id
            );
            assert_eq!(final_state["tasks"]["accept-final"]["candidate"], repair_id);
            assert_eq!(
                final_state["tasks"]["accept-final"]["lineage"]["input"],
                repair_id
            );
            assert_eq!(
                parent["artifacts"][development_id]["object"],
                *development_artifact
            );
            assert_eq!(parent["artifacts"][repair_id]["object"], repair_output);
            let failed_session = failed_state["tasks"]["accept-first"]["verdict"]["envelope"]
                ["reviewer"]["session"]
                .as_str()
                .unwrap()
                .to_string();
            let final_session = final_state["tasks"]["accept-final"]["verdict"]["envelope"]
                ["reviewer"]["session"]
                .as_str()
                .unwrap()
                .to_string();
            let repair_session = repair_session.as_str().unwrap().to_string();
            let developer_session = development_result.as_ref().unwrap()["session"]
                .as_str()
                .unwrap()
                .to_string();
            let coordinator = registered["session"].as_str().unwrap();
            for session in [
                &failed_session,
                &final_session,
                &repair_session,
                &developer_session,
            ] {
                assert_ne!(session.as_str(), coordinator);
            }
            assert_ne!(failed_session, final_session);
            assert_ne!(failed_session, repair_session);
            assert_ne!(failed_session, developer_session);
            assert_ne!(final_session, repair_session);
            assert_ne!(final_session, developer_session);
            assert_ne!(repair_session, developer_session);
            delivery_artifact = Some(repair_output);
        } else {
            native_acceptance_stage(
                &mut env,
                &q,
                &guard,
                &registered,
                development_result.as_ref().unwrap(),
                reviewer_root.as_ref().unwrap(),
            );
        }
        let rows: J = client
            .get(format!("{endpoint}/session/{session_id}/message?limit=200"))
            .query("directory", cwd.to_str().unwrap())
            .call()
            .unwrap()
            .body_mut()
            .with_config()
            .limit(8 * 1024 * 1024)
            .read_json()
            .unwrap();
        let expected_commands = if apply_repair { 4 } else { 2 };
        for action in ["queue ack", "queue task-apply", "queue readiness"] {
            let count = rows
                .as_array()
                .unwrap()
                .iter()
                .filter(|row| row["info"]["sessionID"] == session_id)
                .flat_map(|row| row["parts"].as_array().into_iter().flatten())
                .filter(|part| {
                    part["type"] == "tool"
                        && part["tool"] == "bash"
                        && part["state"]["status"] == "completed"
                        && part["state"]["input"]["command"]
                            .as_str()
                            .is_some_and(|c| c.contains(action) && c.contains(q.to_str().unwrap()))
                })
                .count();
            assert!(
                count >= expected_commands,
                "all native notifications require completed {action} invocations"
            );
        }
    }
    let inventory = apply_delivery.then(|| {
        super::native_inventory::NativeInventory::new(
            &env,
            &q,
            base,
            excluded.to_vec(),
            runtime_roots.clone(),
            expected_tasks.clone(),
            notifications,
        )
    });
    if apply_unknown_delivery {
        queue_child.kill().unwrap();
        queue_child.wait().unwrap();
        let artifact = delivery_artifact.as_ref().unwrap();
        let unknown_evidence = base.join("unknown-delivery-journal.json");
        let confirmation_evidence = base.join("unknown-delivery-confirmation.json");
        failure_evidence.0.push(unknown_evidence.clone());
        failure_evidence.0.push(confirmation_evidence.clone());
        external_unknown_delivery(
            env,
            &q,
            &mut server,
            &mut guard_child,
            DeliverySpec {
                q: &q,
                target: formal_target,
                user_asset,
                artifact,
                expected: delivery_expected.as_ref().unwrap(),
                user_identity: user_identity.unwrap(),
                inventory: inventory.expect("unknown delivery requires inventory"),
            },
            &unknown_evidence,
            &confirmation_evidence,
        );
        let selected_config_after =
            fs::read(std::env::var("HERDR_LIVE_OPENCODE_CONFIG").unwrap()).unwrap();
        assert!(
            selected_config_after
                == *selected_config_before
                    .as_ref()
                    .expect("unknown-delivery config snapshot"),
            "selected OpenCode configuration changed during unknown-delivery scenario"
        );
        return;
    }
    if apply_restart {
        queue_child.kill().unwrap();
        queue_child.wait().unwrap();
        let artifact = delivery_artifact.as_ref().unwrap();
        let inventory = inventory.expect("restart requires inventory");
        external_supervisor_restart(
            env,
            &q,
            &mut server,
            &mut guard_child,
            RestartSpec {
                target: formal_target,
                user_asset,
                artifact,
                expected: delivery_expected.as_ref().unwrap(),
                user_identity: user_identity.unwrap(),
                inventory,
            },
        );
        return;
    }
    if apply_delivery {
        // The native coordinator has finished both notifications. Stop the queue
        // watcher before the external driver takes the queue lock for delivery.
        queue_child.kill().unwrap();
        queue_child.wait().unwrap();
        let artifact = delivery_artifact.as_ref().unwrap();
        let (inventory, expected_inventory) = external_driver_delivery(
            &mut env,
            DeliverySpec {
                q: &q,
                target: formal_target,
                user_asset,
                artifact,
                expected: delivery_expected.as_ref().unwrap(),
                user_identity: user_identity.unwrap(),
                inventory: inventory.expect("delivery requires inventory"),
            },
        );
        for child in [&mut server, &mut guard_child] {
            child.wait().unwrap();
        }
        inventory.after(&expected_inventory);
    } else {
        destroy_environment(&env).unwrap();
        wait_bounded(&mut server, "native server");
        wait_bounded(&mut guard_child, "guard watcher");
        wait_bounded(&mut queue_child, "queue watcher");
    }
    assert!(!env.workspace_root.exists());
    assert!(!env.temp_root.exists());
    assert_eq!(occupied_slots().unwrap(), 0);
}

fn exact_original_event(state: &J) -> Option<(String, J)> {
    let events = state["events"].as_object()?;
    let mut matches = events.iter().filter(|(_, event)| {
        event["task"] == "review" && event["wait"] == "review" && event["kind"] != "progress_stall"
    });
    let (id, event) = matches.next()?;
    assert!(matches.next().is_none(), "multiple original review events");
    Some((id.clone(), event.clone()))
}

fn exact_stall_events(state: &J) -> Vec<(String, J)> {
    state["events"]
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(_, event)| {
            event["task"] == "review"
                && event["wait"] == "review"
                && event["kind"] == "progress_stall"
        })
        .map(|(id, event)| (id.clone(), event.clone()))
        .collect()
}

fn delivery_for_event<'a>(state: &'a J, session: &str, event_id: &str) -> Option<(&'a str, &'a J)> {
    state["deliveries"]
        .as_object()?
        .iter()
        .find(|(_, delivery)| {
            delivery["session"] == session
                && delivery["events"].as_array().is_some_and(|events| {
                    events.iter().any(|event| event.as_str() == Some(event_id))
                })
        })
        .map(|(id, delivery)| (id.as_str(), delivery))
}

struct ToolCommand {
    command: String,
    output: String,
    error: String,
    success: bool,
    status: String,
    exit: Option<i64>,
    start: i64,
    end: i64,
}

fn permission_rows(client: &ureq::Agent, endpoint: &str) -> J {
    client
        .get(format!("{endpoint}/permission"))
        .call()
        .unwrap()
        .body_mut()
        .with_config()
        .limit(1024 * 1024)
        .read_json()
        .unwrap()
}

fn question_rows(client: &ureq::Agent, endpoint: &str) -> J {
    client
        .get(format!("{endpoint}/question"))
        .call()
        .unwrap()
        .body_mut()
        .with_config()
        .limit(1024 * 1024)
        .read_json()
        .unwrap()
}

fn bounded_permission_snapshot(permissions: &J, session: &str) -> J {
    let rows: Vec<_> = permissions
        .as_array()
        .into_iter()
        .flatten()
        .filter(|permission| permission["sessionID"] == session)
        .collect();
    let total = rows.len();
    let field_truncated = rows.iter().take(8).any(|permission| {
        ["pattern", "patterns", "title"]
            .iter()
            .any(|key| match &permission[*key] {
                J::String(text) => text.chars().count() > 512,
                J::Array(items) => {
                    items.len() > 8
                        || items
                            .iter()
                            .take(8)
                            .filter_map(J::as_str)
                            .any(|text| text.chars().count() > 512)
                }
                _ => false,
            })
    });
    let bounded: Vec<_> = rows
        .into_iter()
        .take(8)
        .map(|permission| {
            let strings = |value: &J| -> Vec<String> {
                match value {
                    J::String(text) => vec![text.chars().take(512).collect()],
                    J::Array(items) => items
                        .iter()
                        .take(8)
                        .filter_map(J::as_str)
                        .map(|text| text.chars().take(512).collect())
                        .collect(),
                    _ => Vec::new(),
                }
            };
            json!({
                "id": permission["id"],
                "sessionID": permission["sessionID"],
                "permission": permission["permission"],
                "type": permission["type"],
                "pattern": strings(&permission["pattern"]),
                "patterns": strings(&permission["patterns"]),
                "messageID": permission["messageID"],
                "callID": permission["callID"],
                "tool_messageID": permission["tool"]["messageID"],
                "tool_callID": permission["tool"]["callID"],
                "title": permission["title"].as_str().unwrap_or_default().chars().take(512).collect::<String>(),
                "created": permission["time"]["created"],
            })
        })
        .collect();
    let mut snapshot = json!({"session":session,"total":total,"truncated":total>8 || field_truncated,"permissions":bounded});
    while serde_json::to_vec_pretty(&snapshot).unwrap().len() > 60 * 1024 {
        snapshot["permissions"].as_array_mut().unwrap().pop();
        snapshot["truncated"] = json!(true);
    }
    snapshot
}

#[test]
fn permission_snapshot_is_byte_bounded_and_reports_truncation() {
    let rows = json!((0..8)
        .map(|index| json!({"id":index,"sessionID":"ses-probe",
        "patterns":vec!["\u{0001}".repeat(512);8]}))
        .collect::<Vec<_>>());
    let snapshot = bounded_permission_snapshot(&rows, "ses-probe");
    assert_eq!(snapshot["total"], 8);
    assert_eq!(snapshot["truncated"], true);
    assert!(serde_json::to_vec_pretty(&snapshot).unwrap().len() <= 60 * 1024);
    let long_field = json!([{"sessionID":"ses-probe","patterns":["界".repeat(513)]}]);
    assert_eq!(
        bounded_permission_snapshot(&long_field, "ses-probe")["truncated"],
        true
    );
}

fn permission_patterns(permission: &J) -> Vec<String> {
    let mut patterns = Vec::new();
    for key in ["patterns", "pattern"] {
        match &permission[key] {
            J::String(text) => patterns.push(text.clone()),
            J::Array(items) => {
                patterns.extend(items.iter().filter_map(J::as_str).map(String::from))
            }
            _ => {}
        }
    }
    patterns
}

fn permission_probe_pattern(sentinel: &Path) -> String {
    format!(
        "{}/*",
        sentinel
            .parent()
            .expect("sentinel parent")
            .to_str()
            .unwrap()
    )
}

fn permission_request_fields(
    permission: &J,
    session: &str,
    sentinel: &Path,
) -> Result<(String, String), String> {
    if permission["sessionID"] != session {
        return Err("permission session mismatch".into());
    }
    if !permission["id"]
        .as_str()
        .is_some_and(|value| !value.trim().is_empty())
    {
        return Err("permission request id missing".into());
    }
    let permission_type = permission["permission"]
        .as_str()
        .or_else(|| permission["type"].as_str())
        .unwrap_or_default();
    if permission_type != "external_directory" {
        return Err("permission type is not external_directory".into());
    }
    if permission_patterns(permission) != vec![permission_probe_pattern(sentinel)] {
        return Err("permission pattern is not the exact dedicated sentinel directory".into());
    }
    let message = permission["tool"]["messageID"]
        .as_str()
        .or_else(|| permission["messageID"].as_str())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "permission tool message missing".to_string())?;
    let call_id = permission["tool"]["callID"]
        .as_str()
        .or_else(|| permission["callID"].as_str())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "permission call id missing".to_string())?;
    Ok((message.to_string(), call_id.to_string()))
}

fn exact_permission_binding(
    permissions: &J,
    client: &ureq::Agent,
    endpoint: &str,
    session: &str,
    message: &str,
    cwd: &Path,
    sentinel: &Path,
) -> (J, J) {
    let rows = permissions.as_array().expect("permission rows");
    let matching: Vec<_> = rows
        .iter()
        .filter(|permission| permission["sessionID"] == session)
        .collect();
    assert_eq!(
        matching.len(),
        1,
        "exactly one permission for the reviewer session"
    );
    let permission = matching[0];
    let (tool_message, call_id) = permission_request_fields(permission, session, sentinel)
        .expect("permission request schema");
    let sentinel_text = sentinel.to_str().unwrap();
    let messages = session_rows(client, endpoint, session, cwd);
    let parts: Vec<_> = messages
        .as_array()
        .expect("reviewer messages")
        .iter()
        .flat_map(|row| row["parts"].as_array().into_iter().flatten())
        .filter(|part| {
            part["type"] == "tool"
                && part["sessionID"] == session
                && part["callID"] == call_id
                && part["messageID"] == tool_message
        })
        .collect();
    assert_eq!(parts.len(), 1, "exactly one tool call for the permission");
    let tool = parts[0];
    let assistant: Vec<_> = messages
        .as_array()
        .expect("reviewer messages")
        .iter()
        .filter(|row| {
            row["info"]["sessionID"] == session
                && row["info"]["role"] == "assistant"
                && row["info"]["id"] == tool_message
                && row["info"]["parentID"] == message
        })
        .collect();
    assert_eq!(
        assistant.len(),
        1,
        "permission tool call is not bound to the exact native submission"
    );
    assert_eq!(tool["tool"], "bash");
    assert_eq!(
        tool["state"]["input"]["command"]
            .as_str()
            .expect("permission tool command")
            .trim(),
        format!("cat {sentinel_text}")
    );
    (permission.clone(), tool.clone())
}

fn tool_commands(rows: &J, session: &str) -> Vec<ToolCommand> {
    let mut commands: Vec<ToolCommand> = rows
        .as_array()
        .into_iter()
        .flatten()
        .filter(|row| row["info"]["sessionID"] == session)
        .flat_map(|row| row["parts"].as_array().into_iter().flatten())
        .filter(|part| part["type"] == "tool" && part["tool"] == "bash")
        .filter_map(|part| {
            let command = part["state"]["input"]["command"].as_str()?.to_string();
            let status = part["state"]["status"].as_str().unwrap_or_default();
            let exit = part["state"]["metadata"]["exit"].as_i64();
            let start = part["state"]["time"]["start"].as_i64().unwrap_or(i64::MAX);
            let end = part["state"]["time"]["end"].as_i64().unwrap_or(start);
            Some(ToolCommand {
                command,
                output: part["state"]["output"]
                    .as_str()
                    .or_else(|| part["state"]["metadata"]["output"].as_str())
                    .unwrap_or_default()
                    .to_string(),
                error: part["state"]["error"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
                success: status == "completed" && exit == Some(0),
                status: status.to_string(),
                exit,
                start,
                end,
            })
        })
        .collect();
    commands.sort_by_key(|command| (command.start, command.end));
    commands
}

fn ensure_session(value: &str, label: &str) {
    assert!(!value.trim().is_empty(), "{label} session is empty");
}

fn concurrent_tool_evidence(roles: [(&str, Vec<ToolCommand>); 2]) -> J {
    let mut records = Vec::new();
    let mut counts = serde_json::Map::new();
    let mut truncated = false;
    for (role, commands) in roles {
        let matching: Vec<_> = commands
            .into_iter()
            .filter(|c| {
                c.command.contains("git commit") || c.command.contains("git show HEAD:file")
            })
            .collect();
        let count = matching.len();
        counts.insert(
            role.to_string(),
            json!({"total":count,"omitted":count.saturating_sub(8)}),
        );
        truncated |= count > 8;
        for c in matching.into_iter().rev().take(8) {
            truncated |= c.command.chars().count() > 512;
            records.push(json!({"role":role,
                "command":c.command.chars().take(512).collect::<String>(),
                "exit":c.exit,"status":c.status,"start":c.start,"end":c.end}));
        }
    }
    records.sort_by_key(|record| record["start"].as_i64().unwrap_or(i64::MAX));
    let mut evidence = bounded_restore_tool_evidence(json!({"commands":records,"counts":counts,
        "truncated":truncated,"limit_per_role":8,"command_char_limit":512}));
    for role in ["reviewer", "developer"] {
        let retained = evidence["commands"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|record| record["role"] == role)
            .count();
        let total = evidence["counts"][role]["total"].as_u64().unwrap();
        evidence["counts"][role]["omitted"] = json!(total.saturating_sub(retained as u64));
    }
    evidence
}

#[test]
fn concurrent_diagnostic_records_truncation_and_keeps_newest_commands() {
    let commands = (0..10)
        .map(|index| ToolCommand {
            command: format!("git commit {index} {}", "界".repeat(520)),
            output: "not retained".into(),
            error: "not retained".into(),
            success: true,
            status: "completed".into(),
            exit: Some(0),
            start: index,
            end: index + 1,
        })
        .collect();
    let evidence = concurrent_tool_evidence([("reviewer", Vec::new()), ("developer", commands)]);
    assert_eq!(evidence["counts"]["developer"]["total"], 10);
    assert_eq!(evidence["counts"]["developer"]["omitted"], 2);
    assert_eq!(evidence["truncated"], true);
    assert_eq!(evidence["commands"].as_array().unwrap().len(), 8);
    assert!(evidence["commands"][7]["command"]
        .as_str()
        .unwrap()
        .starts_with("git commit 9 "));
    for command in evidence["commands"].as_array().unwrap() {
        assert!(command["command"].as_str().unwrap().chars().count() <= 512);
        assert!(command.get("output").is_none() && command.get("error").is_none());
    }
    assert!(serde_json::to_vec_pretty(&evidence).unwrap().len() <= 60 * 1024);
}

fn submission_created_millis(value: &J, label: &str) -> f64 {
    let seconds = value
        .as_f64()
        .filter(|value| value.is_finite() && *value > 0.0)
        .unwrap_or_else(|| panic!("{label} created_at missing, nonfinite or nonpositive"));
    let millis = seconds * 1000.0;
    assert!(
        millis.is_finite(),
        "{label} created_at milliseconds overflow"
    );
    millis
}

fn exact_terminal_completion_millis(rows: &J, submission: &J, cwd: &Path, label: &str) -> f64 {
    let session = submission["session"]
        .as_str()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| panic!("{label} submission session missing"));
    let message = submission["message_id"]
        .as_str()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| panic!("{label} submission message ID missing"));
    let reply = submission["reply_id"]
        .as_str()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| panic!("{label} terminal reply ID missing"));
    let cwd = cwd
        .to_str()
        .unwrap_or_else(|| panic!("{label} cwd is not UTF-8"));
    let matches: Vec<_> = rows
        .as_array()
        .expect("native session rows")
        .iter()
        .filter(|row| {
            row["info"]["sessionID"] == session
                && row["info"]["role"] == "assistant"
                && row["info"]["id"] == reply
                && row["info"]["parentID"] == message
                && row["info"]["path"]["cwd"] == cwd
        })
        .collect();
    assert_eq!(
        matches.len(),
        1,
        "{label} requires exactly one terminal assistant row for the exact reply/message/session/cwd"
    );
    matches[0]["info"]["time"]["completed"]
        .as_f64()
        .filter(|value| value.is_finite() && *value > 0.0)
        .unwrap_or_else(|| panic!("{label} assistant completion missing, nonfinite or nonpositive"))
}

fn assert_native_turn_overlap(
    left_label: &str,
    left_created: f64,
    left_completed: f64,
    right_label: &str,
    right_created: f64,
    right_completed: f64,
) {
    assert!(
        left_created < left_completed,
        "{left_label} submission/completion timestamps are reversed"
    );
    assert!(
        right_created < right_completed,
        "{right_label} submission/completion timestamps are reversed"
    );
    let overlap_start = left_created.max(right_created);
    let overlap_end = left_completed.min(right_completed);
    assert!(
        overlap_start < overlap_end
            && left_created < right_completed
            && right_created < left_completed,
        "native reviewer and developer turns did not actually overlap"
    );
}

#[test]
fn concurrent_timestamp_binding_rejects_bad_identity_and_allows_multi_tool_rows() {
    let cwd = Path::new("/tmp/native-concurrent");
    let submission = json!({"session":"ses-a","message_id":"msg-a","reply_id":"reply-a"});
    let rows = json!([
        {"info":{"sessionID":"ses-a","role":"assistant","id":"tool-row","parentID":"msg-a",
            "path":{"cwd":"/tmp/native-concurrent"},"time":{"completed":2000}}},
        {"info":{"sessionID":"ses-a","role":"assistant","id":"reply-a","parentID":"msg-a",
            "path":{"cwd":"/tmp/native-concurrent"},"time":{"completed":3000}}}
    ]);
    assert_eq!(
        exact_terminal_completion_millis(&rows, &submission, cwd, "reviewer"),
        3000.0
    );
    let reject = |f: &dyn Fn()| {
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).is_err());
    };
    let mut wrong_parent = rows.clone();
    wrong_parent[1]["info"]["parentID"] = json!("other-message");
    reject(&|| {
        exact_terminal_completion_millis(&wrong_parent, &submission, cwd, "reviewer");
    });
    let mut wrong_session = rows.clone();
    wrong_session[1]["info"]["sessionID"] = json!("other-session");
    reject(&|| {
        exact_terminal_completion_millis(&wrong_session, &submission, cwd, "reviewer");
    });
    let mut wrong_id = submission.clone();
    wrong_id["reply_id"] = json!("other-reply");
    reject(&|| {
        exact_terminal_completion_millis(&rows, &wrong_id, cwd, "reviewer");
    });
    let mut wrong_cwd = rows.clone();
    wrong_cwd[1]["info"]["path"]["cwd"] = json!("/tmp/other");
    reject(&|| {
        exact_terminal_completion_millis(&wrong_cwd, &submission, cwd, "reviewer");
    });
    let mut missing_completion = rows.clone();
    missing_completion[1]["info"]["time"] = json!({});
    reject(&|| {
        exact_terminal_completion_millis(&missing_completion, &submission, cwd, "reviewer");
    });
    let mut nonfinite_completion = rows.clone();
    nonfinite_completion[1]["info"]["time"]["completed"] = json!("NaN");
    reject(&|| {
        exact_terminal_completion_millis(&nonfinite_completion, &submission, cwd, "reviewer");
    });
    reject(&|| {
        submission_created_millis(&json!({}), "reviewer submission");
    });
    reject(&|| {
        submission_created_millis(&json!(f64::MAX), "reviewer submission");
    });
    reject(&|| {
        assert_native_turn_overlap("left", 3000.0, 2000.0, "right", 1000.0, 4000.0);
    });
    reject(&|| {
        assert_native_turn_overlap("left", 1000.0, 2000.0, "right", 3000.0, 4000.0);
    });
    reject(&|| {
        assert_native_turn_overlap("left", 1000.0, 2000.0, "right", 2000.0, 3000.0);
    });
}

fn assert_concurrent_event_chain(
    q: &Path,
    state: &J,
    wait: &str,
    submission: &J,
    result: &J,
    wait_before: &J,
    root: &Path,
) {
    let session = result["session"]
        .as_str()
        .filter(|value| !value.is_empty())
        .expect("concurrent result session");
    let message = result["message_id"]
        .as_str()
        .filter(|value| !value.is_empty())
        .expect("concurrent result message id");
    let expected_binding = progress::binding(wait_before).unwrap();
    assert_eq!(submission["context"]["wait"], wait);
    assert_eq!(result["context"]["wait"], wait);
    assert_eq!(submission["context"]["binding"], expected_binding);
    assert_eq!(result["context"]["binding"], expected_binding);
    assert_eq!(submission["context"]["queue_id"], state["id"]);
    assert_eq!(result["context"]["queue_id"], state["id"]);
    assert_eq!(
        Path::new(submission["context"]["queue_dir"].as_str().unwrap()),
        q
    );
    assert_eq!(
        Path::new(result["context"]["queue_dir"].as_str().unwrap()),
        q
    );
    assert_eq!(submission["session"], session);
    assert_eq!(submission["message_id"], message);
    assert_eq!(result["session"], session);
    assert_eq!(result["message_id"], message);
    let events: Vec<_> = state["events"]
        .as_object()
        .expect("queue events")
        .iter()
        .filter(|(_, event)| event["wait"] == wait)
        .collect();
    assert_eq!(events.len(), 1, "exactly one event for {wait}");
    let event = events[0].1;
    assert_eq!(event["task"], wait);
    assert_eq!(event["binding"], expected_binding);
    assert_eq!(
        event["evidence"],
        json!(q.join("results").join(format!("{wait}.json")))
    );
    assert_ne!(event["acknowledged"], true);
    assert_ne!(event["applied"], true);
    let signal = util::read_json(Path::new(
        event["evidence"].as_str().expect("event evidence"),
    ))
    .unwrap();
    assert_eq!(signal["wait"], wait);
    assert_eq!(signal["outcome"], "complete");
    let submission_path = root.join("submissions").join(format!("{message}.json"));
    assert_eq!(signal["evidence"], json!(submission_path));
    let actual_submission = util::read_json(&submission_path).unwrap();
    assert_eq!(actual_submission["message_id"], submission["message_id"]);
    assert_eq!(actual_submission["created_at"], submission["created_at"]);
    assert_eq!(actual_submission, *result);
    assert_eq!(actual_submission["reply_id"], result["reply_id"]);
}

fn exact_path_argument(command: &str, flag: &str, path: &Path) -> bool {
    if command.matches(flag).count() != 1 {
        return false;
    }
    let value = command.split_once(flag).unwrap().1.trim_start();
    let path = path.to_str().unwrap();
    [path.to_string(), format!("'{path}'"), format!("\"{path}\"")]
        .iter()
        .any(|form| {
            value
                .strip_prefix(form.as_str())
                .is_some_and(|suffix| suffix.is_empty() || suffix.starts_with(char::is_whitespace))
        })
}

fn terminal_tool_state(command: &ToolCommand) -> bool {
    matches!(command.status.as_str(), "completed" | "error")
}

fn lock_busy_evidence(command: &ToolCommand) -> bool {
    let evidence = format!("{} {}", command.output, command.error).to_ascii_lowercase();
    terminal_tool_state(command)
        && command.exit.is_some_and(|exit| exit != 0)
        && (evidence.contains("wait queue busy; retry after current poll")
            || evidence.contains("opencode submission is busy"))
}

fn stale_apply_evidence(command: &ToolCommand) -> bool {
    terminal_tool_state(command)
        && command.exit.is_some_and(|exit| exit != 0)
        && format!("{} {}", command.output, command.error)
            .contains("obsolete requirement, candidate or acceptance round, or inactive parent")
}

fn session_rows(client: &ureq::Agent, endpoint: &str, session: &str, cwd: &Path) -> J {
    client
        .get(format!("{endpoint}/session/{session}/message?limit=200"))
        .query("directory", cwd.to_str().unwrap())
        .call()
        .unwrap()
        .body_mut()
        .with_config()
        .limit(8 * 1024 * 1024)
        .read_json()
        .unwrap()
}

fn artifact_ids(state: &J) -> std::collections::BTreeSet<String> {
    state["tasks"]["project"]["artifacts"]
        .as_object()
        .map(|artifacts| artifacts.keys().cloned().collect())
        .unwrap_or_default()
}

struct AckStallRecoverySpec<'a> {
    q: &'a Path,
    guard: &'a Path,
    registered: &'a J,
    client: &'a ureq::Agent,
    endpoint: &'a str,
    cwd: &'a Path,
    developer_release: &'a Path,
    result: &'a J,
    snapshot_path: &'a Path,
}

fn verify_ack_stall_recovery(spec: AckStallRecoverySpec<'_>) {
    let AckStallRecoverySpec {
        q,
        guard,
        registered,
        client,
        endpoint,
        cwd,
        developer_release,
        result,
        snapshot_path,
    } = spec;
    let session_id = registered["session"]
        .as_str()
        .expect("registered receiver session");
    let worker_session = result["session"]
        .as_str()
        .filter(|session| !session.is_empty())
        .expect("worker result session");
    assert_ne!(
        worker_session, session_id,
        "worker must not be the coordinator"
    );
    let output = &result["output"];
    let output_id = output["id"].as_str().expect("developer output id");
    assert_ne!(output_id, output["input"].as_str().unwrap_or_default());
    let deadline = Instant::now() + Duration::from_secs(180);
    let (original_id, initial_delivery, snapshot) = loop {
        let state = queue::load_queue(q).unwrap();
        if let Some((id, event)) = exact_original_event(&state) {
            assert_ne!(
                event["applied"], true,
                "ACK-only phase was not observed; original event applied before ACK snapshot"
            );
            if event["acknowledged"] == true {
                if let Some((delivery, record)) = delivery_for_event(&state, session_id, &id) {
                    if record["phase"] == "acknowledged" {
                        let task = &state["tasks"]["review"];
                        assert_eq!(task["phase"], "result_pending");
                        assert!(task["pending"]
                            .as_array()
                            .is_some_and(|pending| pending.iter().any(|event| event == &id)));
                        assert!(state["tasks"]["project"]["approved"].is_null());
                        assert!(!artifact_ids(&state).contains(output_id));
                        assert!(
                            exact_stall_events(&state).is_empty(),
                            "progress_stall existed before the ACK-only snapshot"
                        );
                        assert_eq!(
                            state["deliveries"].as_object().map(|items| items.len()),
                            Some(1),
                            "ACK-only snapshot requires exactly one delivery"
                        );
                        assert_eq!(record["events"], json!([id.as_str()]));
                        let signal = util::read_json(Path::new(
                            event["evidence"].as_str().expect("result evidence path"),
                        ))
                        .unwrap();
                        let worker_report = signal["evidence"]
                            .as_str()
                            .expect("worker collector report path");
                        let report = util::read_json(Path::new(worker_report)).unwrap();
                        assert_eq!(report["session"], json!(worker_session));
                        assert_eq!(report["output"], *output);
                        let snapshot_rows = session_rows(client, endpoint, session_id, cwd);
                        let snapshot_commands = tool_commands(&snapshot_rows, session_id);
                        let first_turn_task_apply = snapshot_commands
                            .iter()
                            .filter(|command| command.command.contains("queue task-apply"))
                            .count();
                        assert_eq!(
                            first_turn_task_apply, 0,
                            "first coordinator turn ran task-apply instead of stopping after ACK"
                        );
                        let snapshot = json!({
                            "observed_at": util::now(),
                            "coordinator_session": session_id,
                            "worker_session": worker_session,
                            "delivery": delivery,
                            "delivery_phase": record["phase"],
                            "delivery_status": state["delivery_status"],
                            "active": state["active"],
                            "original_event_id": id,
                            "original_event": event,
                            "original_binding": event["binding"],
                            "original_output": output,
                            "original_acknowledged": event["acknowledged"],
                            "original_applied": event["applied"],
                            "worker_report": worker_report,
                            "task_state_version": task["state_version"],
                            "task_phase": task["phase"],
                            "pending": task["pending"],
                            "artifact_ids": artifact_ids(&state),
                            "first_turn_no_task_apply": true,
                            "first_turn_task_apply_commands": first_turn_task_apply,
                            "first_turn_commands_observed": snapshot_commands.len(),
                        });
                        util::atomic_write_json(snapshot_path, &snapshot).unwrap();
                        break (id, delivery.to_string(), snapshot);
                    }
                }
            }
        }
        assert!(
            Instant::now() < deadline,
            "ACK-only coordinator phase was not observed within 180s"
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    let expected_binding = result["context"]["binding"].clone();
    let snapshot_on_disk = util::read_json(snapshot_path).unwrap();
    assert_eq!(snapshot_on_disk, snapshot);
    assert_eq!(snapshot_on_disk["original_binding"], expected_binding);
    assert_eq!(
        snapshot_on_disk["original_event_id"].as_str(),
        Some(original_id.as_str())
    );
    assert_eq!(snapshot_on_disk["original_output"], *output);
    assert_eq!(snapshot_on_disk["original_applied"], false);
    assert_eq!(snapshot_on_disk["first_turn_no_task_apply"], true);
    assert_eq!(snapshot_on_disk["first_turn_task_apply_commands"], 0);
    assert_eq!(snapshot_on_disk["coordinator_session"], session_id);
    assert_eq!(snapshot_on_disk["worker_session"], worker_session);
    let stall_id = loop {
        let state = queue::load_queue(q).unwrap();
        let stalls = exact_stall_events(&state);
        assert!(stalls.len() <= 1, "multiple stall events for one task");
        if let Some((id, event)) = stalls.first() {
            assert_eq!(event["kind"], "progress_stall");
            assert_eq!(event["reason"], "result_unapplied");
            assert_eq!(event["wait"], "review");
            assert_eq!(event["task"], "review");
            assert_eq!(event["state_version"], snapshot["task_state_version"]);
            assert_eq!(event["binding"], snapshot["original_binding"]);
            let original = &state["events"][original_id.as_str()];
            let task = &state["tasks"]["review"];
            assert_eq!(original, &snapshot["original_event"]);
            assert_eq!(original["acknowledged"], true);
            assert_eq!(original["applied"], false);
            assert_eq!(task["phase"], "result_pending");
            assert!(task["pending"]
                .as_array()
                .is_some_and(|pending| pending.iter().any(|item| item == &original_id)));
            break id.clone();
        }
        assert!(
            Instant::now() < deadline,
            "progress_stall was not emitted within 180s"
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    assert_ne!(original_id, stall_id);

    let state = loop {
        let state = queue::load_queue(q).unwrap();
        let original = &state["events"][original_id.as_str()];
        let initial = &state["deliveries"][initial_delivery.as_str()];
        let Some((stall_delivery, stall_record)) =
            delivery_for_event(&state, session_id, &stall_id)
        else {
            assert!(Instant::now() < deadline, "stall delivery missing");
            std::thread::sleep(Duration::from_millis(100));
            continue;
        };
        let stall_delivery = stall_delivery.to_string();
        let submission_complete = util::read_json(&guard.join("submission.json"))
            .is_ok_and(|submission| submission["phase"] == "complete");
        let readiness = util::read_json(&q.join("readiness.json")).ok();
        let ready = readiness.as_ref().is_some_and(|value| {
            value["active_wakeup"] == true
                && value["last_confirmed_receipt"]["delivery"] == stall_delivery.as_str()
                && value["last_confirmed_receipt"]["session"] == session_id
        });
        let complete = original["applied"] == true
            && state["tasks"]["review"]["phase"] == "completed"
            && state["tasks"]["review"]["pending"] == json!([])
            && state["deliveries"]
                .as_object()
                .is_some_and(|items| items.len() == 2)
            && initial["phase"] == "acknowledged"
            && stall_record["phase"] == "acknowledged"
            && initial["session"] == session_id
            && stall_record["session"] == session_id
            && submission_complete
            && ready;
        if complete {
            assert_eq!(initial["events"], json!([original_id.as_str()]));
            assert_eq!(stall_record["events"], json!([stall_id.as_str()]));
            break (state, stall_delivery);
        }
        assert!(
            Instant::now() < deadline,
            "ACK/stall recovery did not apply the original result within 180s"
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    let (state, stall_delivery) = state;
    let original = &state["events"][original_id.as_str()];
    let stall = &state["events"][stall_id.as_str()];
    let task = &state["tasks"]["review"];
    assert_eq!(original["binding"], expected_binding);
    assert_eq!(original["acknowledged"], true);
    assert_eq!(original["applied"], true);
    assert_eq!(original["applied_version"], task["state_version"]);
    assert_eq!(task["next_action"]["kind"], "complete");
    let readiness = util::read_json(&q.join("readiness.json")).unwrap();
    assert_eq!(readiness["active_wakeup"], true);
    assert_eq!(
        readiness["last_confirmed_receipt"]["delivery"].as_str(),
        Some(stall_delivery.as_str())
    );
    assert_eq!(
        readiness["last_confirmed_receipt"]["session"].as_str(),
        Some(session_id)
    );
    assert!(!stall["applied"].as_bool().unwrap_or(false));
    assert_eq!(state["events"].as_object().unwrap().len(), 2);
    assert_eq!(state["tasks"]["project"]["approved"], J::Null);
    assert_eq!(
        state["tasks"]["project"]["artifacts"][output_id]["object"],
        *output
    );
    let before = snapshot["artifact_ids"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|id| id.as_str().map(str::to_string))
        .collect::<std::collections::BTreeSet<_>>();
    let after = artifact_ids(&state);
    let added: Vec<_> = after.difference(&before).cloned().collect();
    assert_eq!(
        added,
        vec![output_id.to_string()],
        "unexpected final artifacts"
    );
    let output_cwd = Path::new(output["location"].as_str().unwrap());
    assert_eq!(git(output_cwd, &["rev-parse", "HEAD"]), output_id);
    assert_eq!(
        git(output_cwd, &["show", &format!("{output_id}:file")]),
        "version B"
    );
    let receipt = util::read_json(developer_release).unwrap();
    assert_eq!(receipt["session"].as_str(), Some(worker_session));
    assert_eq!(result["session"].as_str(), Some(worker_session));
    assert_eq!(task["last_release"]["evidence"], receipt["status"]);

    let rows = session_rows(client, endpoint, session_id, cwd);
    let commands = tool_commands(&rows, session_id);
    assert!(
        commands
            .windows(2)
            .all(|pair| pair[0].start <= pair[1].start),
        "tool command timestamps are not chronological"
    );
    let queue_path = q.to_str().unwrap();
    let successful_acks: Vec<_> = commands
        .iter()
        .filter(|command| {
            command.success
                && command.command.contains("queue ack")
                && command.command.contains(queue_path)
        })
        .collect();
    assert!(
        successful_acks.len() >= 2,
        "both deliveries require successful ACK calls"
    );
    assert!(
        successful_acks
            .iter()
            .any(|command| command.command.contains(initial_delivery.as_str())),
        "initial delivery ACK missing"
    );
    assert!(
        successful_acks
            .iter()
            .any(|command| command.command.contains(stall_delivery.as_str())),
        "stall delivery ACK missing"
    );
    let task_apply_attempts: Vec<_> = commands
        .iter()
        .filter(|command| {
            command.command.contains("queue task-apply") && command.command.contains(queue_path)
        })
        .collect();
    let successful_task_apply: Vec<_> = task_apply_attempts
        .iter()
        .filter(|command| command.success)
        .collect();
    assert_eq!(
        successful_task_apply.len(),
        1,
        "exactly one successful task-apply required"
    );
    for command in task_apply_attempts
        .iter()
        .filter(|command| !command.success)
    {
        let evidence = format!("{} {}", command.output, command.error).to_ascii_lowercase();
        assert!(
            evidence.contains("wait queue busy; retry after current poll")
                || evidence.contains("opencode submission is busy"),
            "unsuccessful task-apply without lock-busy evidence: {}",
            command.command
        );
    }
    let stall_ack = commands
        .iter()
        .position(|command| {
            command.success
                && command.command.contains("queue ack")
                && command.command.contains(stall_delivery.as_str())
                && command.command.contains(queue_path)
        })
        .expect("successful stall ACK command missing");
    let event_current = commands
        .iter()
        .position(|command| {
            command.success
                && command.command.contains("queue event-current")
                && command.command.contains(stall_id.as_str())
                && command.command.contains(queue_path)
        })
        .expect("event-current was not run successfully for the stall alarm");
    let task_apply = commands
        .iter()
        .position(|command| {
            command.success
                && command.command.contains("queue task-apply")
                && command.command.contains(queue_path)
        })
        .expect("recovery task-apply missing");
    for index in [stall_ack, event_current, task_apply] {
        assert!(
            commands[index].start > 0
                && commands[index].start != i64::MAX
                && commands[index].end >= commands[index].start
                && commands[index].end != i64::MAX,
            "missing or invalid tool timestamps"
        );
    }
    assert!(
        commands[stall_ack].end <= commands[event_current].start,
        "stall ACK must finish before event-current"
    );
    assert!(
        commands[event_current].end <= commands[task_apply].start,
        "event-current must finish before task-apply"
    );
    assert!(stall_ack < task_apply, "stall ACK must precede task-apply");
    assert!(
        event_current < task_apply,
        "event-current preceded recovery application"
    );
    let post_apply_readiness = commands
        .iter()
        .find(|command| {
            command.success
                && command.command.contains("queue readiness")
                && command.command.contains(queue_path)
                && command.start >= commands[task_apply].end
                && command.start != i64::MAX
                && command.end >= command.start
                && command.end != i64::MAX
        })
        .expect("successful readiness after task-apply missing");
    let checked_at = readiness["checked_at"]
        .as_f64()
        .expect("readiness checked_at");
    assert!(
        checked_at * 1000.0 >= post_apply_readiness.start as f64
            && checked_at * 1000.0 <= post_apply_readiness.end as f64 + 1.0,
        "readiness file does not belong to the post-apply invocation"
    );
    let readiness_output: J = serde_json::from_str(post_apply_readiness.output.trim())
        .expect("readiness command JSON output");
    assert_eq!(
        readiness_output, readiness,
        "persisted readiness must match the post-apply command output"
    );
    assert!(
        !commands.iter().any(|command| {
            command.command.contains("queue task-apply")
                && command.command.contains(stall_id.as_str())
        }),
        "progress_stall was applied as a result"
    );
    println!(
        "real OpenCode ACK/stall recovery: ACK-only snapshot, stall {}, original {} applied once",
        stall_id, original_id
    );
}

struct LateAcceptanceSpec<'a> {
    env: &'a mut TaskEnvironment,
    q: &'a Path,
    guard: &'a Path,
    coordinator: &'a J,
    development: &'a J,
    reviewer_root: &'a Path,
    client: &'a ureq::Agent,
    endpoint: &'a str,
    cwd: &'a Path,
    evidence: &'a mut FailureEvidence,
}

fn late_acceptance_stage(spec: LateAcceptanceSpec<'_>) {
    let LateAcceptanceSpec {
        env,
        q,
        guard,
        coordinator,
        development,
        reviewer_root,
        client,
        endpoint,
        cwd,
        evidence,
    } = spec;
    let artifact = &development["output"];
    let candidate_id = artifact["id"].as_str().expect("development output id");
    let coordinator_session = coordinator["session"]
        .as_str()
        .expect("coordinator session");
    acceptance_live::change(
        q,
        json!({"kind":"candidate","candidate":candidate_id,"reason":"fixed snapshot A acceptance"}),
    );
    let mut activity = acceptance_live::activity("accept-native", "acceptance", &artifact["id"]);
    activity["kind"] = json!("activity");
    acceptance_live::change(q, activity);
    let reviewer_cwd = acceptance_live::clone_snapshot(env, artifact, "late-reviewer-checkout");
    let reviewer_report_path = reviewer_cwd.join(".herdr-verdict.json");
    let application_path = cwd.join("late-application.json");
    let reviewer_instruction = "Independently inspect the exact supplied candidate against the original requirement. Do not edit tracked files or commit. Write .herdr-verdict.json following acceptance_protocol with actual checks and grounded evidence. Derive the conclusion yourself. Do not inspect sibling directories, coordinator instructions, or operator repositories; all requirement and acceptance protocol inputs are supplied in your packet.";
    let session_id = coordinator_session.to_string();
    let (reviewer, collected) = acceptance_live::native_with_hook(
        env,
        q,
        &reviewer_cwd,
        "accept-native",
        reviewer_instruction,
        || {
            let output = cli_output(&["queue", "disable", "--state-dir", q.to_str().unwrap()]);
            assert!(
                output.status.success(),
                "queue disable failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        },
        |_, _, _| {},
    );
    assert_ne!(reviewer["session"], coordinator["session"]);
    assert_ne!(reviewer["session"], development["session"]);
    assert_ne!(session_id, reviewer["session"].as_str().unwrap());
    let a_head = git(&reviewer_cwd, &["rev-parse", "HEAD"]);
    assert_eq!(a_head, candidate_id);
    let a_content = fs::read(reviewer_cwd.join("file")).unwrap();
    assert_eq!(a_content, b"version B\n".to_vec());
    let report_bytes = fs::read(&reviewer_report_path).unwrap();
    let actual_report = util::read_json(&reviewer_report_path).unwrap();
    evidence.0.push(reviewer_report_path.clone());

    let event_deadline = Instant::now() + Duration::from_secs(30);
    let state = loop {
        let state = queue::load_queue(q).unwrap();
        assert_eq!(state["notify"], false);
        let count = state["events"]
            .as_object()
            .unwrap()
            .values()
            .filter(|event| event["wait"] == "accept-native")
            .count();
        assert!(count <= 1, "duplicate acceptance event");
        if count == 1 {
            break state;
        }
        assert!(
            Instant::now() < event_deadline,
            "production watcher did not collect the acceptance signal"
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    let accept_events: Vec<_> = state["events"]
        .as_object()
        .unwrap()
        .iter()
        .filter(|(_, event)| event["wait"] == "accept-native")
        .collect();
    assert_eq!(accept_events.len(), 1, "unique acceptance event required");
    let (event_id, event) = {
        let (id, event) = accept_events[0];
        (id.clone(), event.clone())
    };
    let verdict_before = state["tasks"]["accept-native"]["verdict"].clone();
    let child_state_version = state["tasks"]["accept-native"]["state_version"].clone();
    let signal = util::read_json(Path::new(
        event["evidence"].as_str().expect("acceptance evidence"),
    ))
    .unwrap();
    let expected_submission = reviewer_root.join("submissions").join(format!(
        "{}.json",
        collected["message_id"]
            .as_str()
            .expect("reviewer message id")
    ));
    assert_eq!(signal["evidence"], json!(expected_submission));
    let submission_report = util::read_json(&expected_submission).unwrap();
    assert_eq!(submission_report["session"], reviewer["session"]);
    assert_eq!(submission_report["output"], collected["output"]);
    assert_eq!(event["applied"], false);
    assert_eq!(event["acknowledged"], false);
    assert_eq!(event["binding"], collected["context"]["binding"]);
    assert_eq!(
        event["evidence"],
        json!(q.join("results/accept-native.json"))
    );
    assert!(state["tasks"]["accept-native"]["pending"]
        .as_array()
        .is_some_and(|pending| pending.iter().any(|item| item == &event_id)));
    assert!(
        delivery_for_event(&state, coordinator_session, &event_id).is_none(),
        "delivery occurred while notifications were disabled"
    );
    assert_eq!(state["notify"], false);
    let verdict = &state["tasks"]["accept-native"]["verdict"];
    assert_eq!(
        verdict["envelope"]["binding"],
        collected["context"]["binding"]
    );
    assert_eq!(
        verdict["envelope"]["reviewer"]["session"],
        reviewer["session"]
    );
    assert_eq!(verdict["envelope"]["reviewer"]["adapter"], "opencode");
    assert_eq!(verdict["envelope"]["report"], actual_report);
    assert_eq!(
        util::read_json(&reviewer_report_path).unwrap(),
        actual_report
    );

    let b_cwd = acceptance_live::clone_snapshot(env, artifact, "late-candidate");
    git(&b_cwd, &["config", "user.name", "Late fixture"]);
    git(&b_cwd, &["config", "user.email", "late@example.invalid"]);
    fs::write(b_cwd.join("file"), b"version C\n").unwrap();
    git(&b_cwd, &["add", "file"]);
    git(&b_cwd, &["commit", "-qm", "late candidate fixture"]);
    let b_id = git(&b_cwd, &["rev-parse", "HEAD"]);
    assert_ne!(b_id, a_head);
    assert_eq!(
        fs::read(b_cwd.join("file")).unwrap(),
        b"version C\n".to_vec()
    );
    acceptance_live::change(
        q,
        json!({"kind":"artifact","artifact":{"adapter":"git","id":b_id,"location":b_cwd,"input":candidate_id}}),
    );
    acceptance_live::change(
        q,
        json!({"kind":"candidate","candidate":b_id,"reason":"driver-owned late-result candidate fixture"}),
    );

    let state = queue::load_queue(q).unwrap();
    assert_eq!(state["tasks"]["project"]["candidate"], b_id);
    assert_eq!(state["tasks"]["project"]["approved"], J::Null);
    assert_eq!(
        state["tasks"]["accept-native"]["pending"],
        json!([event_id.as_str()])
    );
    assert_eq!(state["tasks"]["accept-native"]["phase"], "result_pending");
    assert!(!progress::current_event(
        &state,
        &state["events"][event_id.as_str()]
    ));
    assert_eq!(git(&reviewer_cwd, &["rev-parse", "HEAD"]), a_head);
    assert_eq!(fs::read(reviewer_cwd.join("file")).unwrap(), a_content);
    assert_eq!(fs::read(&reviewer_report_path).unwrap(), report_bytes);

    let prior_submission = util::read_json(&guard.join("submission.json")).ok();
    let prior_message = prior_submission
        .as_ref()
        .and_then(|submission| submission["message_id"].as_str())
        .map(str::to_string);
    let output = cli_output(&["queue", "enable", "--state-dir", q.to_str().unwrap()]);
    assert!(
        output.status.success(),
        "queue enable failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let deadline = Instant::now() + Duration::from_secs(180);
    let (delivery_id, acknowledged_event) = loop {
        let state = queue::load_queue(q).unwrap();
        if let Some((delivery, record)) = delivery_for_event(&state, coordinator_session, &event_id)
        {
            if record["phase"] == "acknowledged" {
                break (
                    delivery.to_string(),
                    state["events"][event_id.as_str()].clone(),
                );
            }
        }
        assert!(Instant::now() < deadline, "late ACK/delivery deadline");
        std::thread::sleep(Duration::from_millis(100));
    };
    assert_eq!(acknowledged_event["acknowledged"], true);
    assert_eq!(acknowledged_event["applied"], false);
    assert_eq!(
        acknowledged_event["binding"],
        collected["context"]["binding"]
    );
    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        if let Ok(submission) = util::read_json(&guard.join("submission.json")) {
            let changed = submission["message_id"].as_str() != prior_message.as_deref();
            if changed && submission["phase"] == "complete" {
                break;
            }
        }
        assert!(
            Instant::now() < deadline,
            "late coordinator turn did not complete"
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    let deadline = Instant::now() + Duration::from_secs(180);
    let (commands, ack_index, event_current_index, task_apply_index) = loop {
        let rows = session_rows(client, endpoint, coordinator_session, cwd);
        let commands = tool_commands(&rows, coordinator_session);
        let ack = commands.iter().position(|command| {
            command.success
                && command.command.contains("queue ack")
                && command.command.contains(delivery_id.as_str())
                && command.command.contains(q.to_str().unwrap())
        });
        let event_current = commands.iter().position(|command| {
            terminal_tool_state(command)
                && command.exit == Some(1)
                && command.command.contains("queue event-current")
                && command.command.contains(event_id.as_str())
                && command.command.contains(q.to_str().unwrap())
        });
        let task_apply = commands.iter().position(|command| {
            command.command.contains("queue task-apply")
                && command.command.contains(q.to_str().unwrap())
                && stale_apply_evidence(command)
        });
        if let (Some(ack), Some(event_current), Some(task_apply)) = (ack, event_current, task_apply)
        {
            break (commands, ack, event_current, task_apply);
        }
        assert!(
            Instant::now() < deadline,
            "late stale application/tool evidence deadline"
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    for index in [ack_index, event_current_index, task_apply_index] {
        assert!(
            commands[index].start > 0
                && commands[index].start != i64::MAX
                && commands[index].end >= commands[index].start
                && commands[index].end != i64::MAX,
            "missing or invalid late-recovery tool timestamps"
        );
    }
    assert!(commands[ack_index].end <= commands[event_current_index].start);
    assert!(commands[event_current_index].end <= commands[task_apply_index].start);
    let application_command = &commands[task_apply_index].command;
    assert_eq!(
        application_command.matches("--spec").count(),
        1,
        "exactly one application --spec argument required"
    );
    let spec_arg = application_command
        .split_once("--spec")
        .unwrap()
        .1
        .trim_start();
    let application_text = application_path.to_str().unwrap();
    let forms = [
        application_text.to_string(),
        format!("'{application_text}'"),
        format!("\"{application_text}\""),
    ];
    assert!(
        forms.iter().any(|form| spec_arg
            .strip_prefix(form)
            .is_some_and(|suffix| suffix.is_empty() || suffix.starts_with(char::is_whitespace))),
        "stale task-apply must use the verified application file"
    );
    let event_current_output: J = serde_json::from_str(commands[event_current_index].output.trim())
        .expect("event-current JSON output");
    assert_eq!(event_current_output["event"], event_id.as_str());
    assert_eq!(event_current_output["current"], false);
    let late_event_currents: Vec<_> = commands
        .iter()
        .filter(|command| {
            command.start >= commands[ack_index].end
                && command.command.contains("queue event-current")
                && command.command.contains(event_id.as_str())
                && command.command.contains(q.to_str().unwrap())
        })
        .collect();
    assert_eq!(
        late_event_currents.len(),
        1,
        "one late event-current required"
    );
    let late_applies: Vec<_> = commands
        .iter()
        .filter(|command| {
            command.start >= commands[ack_index].end
                && command.command.contains("queue task-apply")
                && command.command.contains(q.to_str().unwrap())
        })
        .collect();
    let stale_attempts: Vec<_> = late_applies
        .iter()
        .filter(|command| stale_apply_evidence(command))
        .collect();
    assert_eq!(
        stale_attempts.len(),
        1,
        "exactly one stale task-apply required"
    );
    assert!(
        !late_applies.iter().any(|command| command.success),
        "stale acceptance application succeeded"
    );
    for command in late_applies
        .iter()
        .filter(|command| !stale_apply_evidence(command))
    {
        assert!(
            lock_busy_evidence(command),
            "non-busy late task-apply rejection: {}",
            command.command
        );
    }
    let state = queue::load_queue(q).unwrap();
    let accept_events: Vec<_> = state["events"]
        .as_object()
        .unwrap()
        .values()
        .filter(|event| event["wait"] == "accept-native")
        .collect();
    assert_eq!(accept_events.len(), 1, "exactly one accept event required");
    assert_eq!(state["events"].as_object().unwrap().len(), 2);
    assert_eq!(state["deliveries"].as_object().unwrap().len(), 2);
    let exact_deliveries: Vec<_> = state["deliveries"]
        .as_object()
        .unwrap()
        .values()
        .filter(|delivery| delivery["events"] == json!([event_id.as_str()]))
        .collect();
    assert_eq!(exact_deliveries.len(), 1);
    assert_eq!(exact_deliveries[0]["session"], coordinator["session"]);
    assert_eq!(exact_deliveries[0]["phase"], "acknowledged");
    let event = &state["events"][event_id.as_str()];
    assert_eq!(event["acknowledged"], true);
    assert_eq!(event["applied"], false);
    assert_eq!(state["tasks"]["accept-native"]["verdict"], verdict_before);
    assert_eq!(
        state["tasks"]["accept-native"]["pending"],
        json!([event_id.as_str()])
    );
    assert_eq!(state["tasks"]["accept-native"]["phase"], "result_pending");
    assert_eq!(state["tasks"]["project"]["candidate"], b_id);
    assert_eq!(state["tasks"]["project"]["approved"], J::Null);
    let application = util::read_json(&application_path).unwrap();
    assert_eq!(application["task"], "accept-native");
    assert_eq!(application["event"].as_str(), Some(event_id.as_str()));
    assert_eq!(application["state_version"], child_state_version);
    assert_eq!(application["verdict"], verdict_before["id"]);
    assert!(application.get("output").is_none());
    assert!(application.get("acceptance").is_none());
    let expected_next = match actual_report["conclusion"].as_str().unwrap() {
        "passed" => "delivery",
        "failed" => "repair",
        "unable" => "restore",
        "ambiguous" => "clarify",
        other => panic!("unexpected reviewer conclusion {other}"),
    };
    assert_eq!(application["next_action"]["kind"], expected_next);
    assert_eq!(application["next_action"]["owner"], "native coordinator");
    assert_eq!(application["next_action"]["reason"], "independent report");
    assert_eq!(
        application["next_action"]["condition"],
        "existing authorization"
    );
    assert!(
        state["events"]
            .as_object()
            .unwrap()
            .values()
            .all(|event| event["wait"] != "accept-native" || event["kind"] != "progress_stall"),
        "immediate pre-deadline A stall observed; overdue suppression is not demonstrated"
    );
    assert_eq!(git(&reviewer_cwd, &["rev-parse", "HEAD"]), a_head);
    assert_eq!(fs::read(reviewer_cwd.join("file")).unwrap(), a_content);
    assert_eq!(fs::read(&reviewer_report_path).unwrap(), report_bytes);
    assert_eq!(git(&b_cwd, &["rev-parse", "HEAD"]), b_id);
    assert_eq!(
        fs::read(b_cwd.join("file")).unwrap(),
        b"version C\n".to_vec()
    );
    println!(
        "real OpenCode fixed-snapshot late result: A {} collected; B {} selected; stale event {} ACKed but not applied",
        candidate_id, b_id, event_id
    );
}

struct RestoredReviewSpec<'a> {
    env: &'a mut TaskEnvironment,
    q: &'a Path,
    guard: &'a Path,
    coordinator: &'a J,
    unable: &'a J,
    artifact: &'a J,
    reviewer_root: &'a Path,
    client: &'a ureq::Agent,
    endpoint: &'a str,
    cwd: &'a Path,
    evidence: &'a mut FailureEvidence,
}

fn register_operator_wait(q: &Path) {
    let _lock = queue::locked(q, true).unwrap();
    queue::add(
        q,
        &json!({
            "id":"restore-dependency-install",
            "task":"project",
            "epoch":1,
            "owner":"operator",
            "reason":"install the disposable operator dependency",
            "condition":"actual install result",
            "kind":"signal",
            "timeout_seconds":600
        }),
    )
    .unwrap();
}

struct BoundedChild {
    child: Option<std::process::Child>,
}

impl BoundedChild {
    fn new(child: std::process::Child) -> Self {
        Self { child: Some(child) }
    }
    fn child_mut(&mut self) -> &mut std::process::Child {
        self.child.as_mut().unwrap()
    }
    fn release(&mut self) {
        self.child.take();
    }
    fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) if Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    Ok(None) => {
                        eprintln!("bounded child did not exit within 5s after kill");
                        break;
                    }
                    Err(error) => {
                        eprintln!("bounded child wait failed after kill: {error}");
                        break;
                    }
                }
            }
        }
    }
    fn wait_status(&mut self) -> std::io::Result<std::process::ExitStatus> {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(status) = self.child_mut().try_wait()? {
                self.release();
                return Ok(status);
            }
            if Instant::now() >= deadline {
                self.stop();
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "bounded child did not exit within 30s",
                ));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for BoundedChild {
    fn drop(&mut self) {
        self.stop();
    }
}

fn install_operator_dependency(
    env: &mut TaskEnvironment,
    checkout: &Path,
    evidence_path: &Path,
) -> std::path::PathBuf {
    let source = env.workspace_root.join("operator-dependency-source");
    fs::write(&source, b"#!/bin/sh\nprintf 'operator-dependency-v1\n'\n").unwrap();
    fs::set_permissions(&source, fs::Permissions::from_mode(0o700)).unwrap();
    let destination = checkout.join(UNAVAILABLE_DEPENDENCY);
    let mut command = env.command(Path::new("/usr/bin/install"));
    command
        .args(["-m", "0700"])
        .arg(&source)
        .arg(&destination)
        .current_dir(checkout);
    let mut guard = BoundedChild::new(command.spawn().unwrap());
    let pid = guard.child_mut().id();
    let failure = |guard: &mut BoundedChild, message: String| -> ! {
        let _ = util::atomic_write_json(
            evidence_path,
            &json!({
                "operator": "test-driver",
                "phase": "failure",
                "pid": pid,
                "message": message,
                "status": guard.child.as_mut().and_then(|child| {
                    child.try_wait().ok().flatten().map(|status| status.code())
                }),
                "path": destination,
                "source": source
            }),
        );
        guard.stop();
        panic!("{message}");
    };
    let mut observed_terminal = None;
    if let Err(error) = env.register_process(pid as i32) {
        match guard.child_mut().try_wait() {
            Ok(Some(status)) if status.success() => {
                guard.release();
                observed_terminal = Some(status);
            }
            Ok(Some(status)) => failure(
                &mut guard,
                format!("operator install exited before registration: {status:?}"),
            ),
            _ => failure(
                &mut guard,
                format!("operator install registration failed: {error}"),
            ),
        }
    }
    let status = if let Some(status) = observed_terminal {
        status
    } else {
        match guard.wait_status() {
            Ok(status) => status,
            Err(error) => failure(&mut guard, format!("operator install wait failed: {error}")),
        }
    };
    if !status.success() {
        failure(&mut guard, format!("operator install exited: {status:?}"));
    }
    let bytes = fs::read(&destination).unwrap_or_else(|error| {
        failure(
            &mut guard,
            format!("installed dependency read failed: {error}"),
        )
    });
    let metadata = fs::metadata(&destination).unwrap_or_else(|error| {
        failure(
            &mut guard,
            format!("installed dependency metadata failed: {error}"),
        )
    });
    if !metadata.is_file() || metadata.permissions().mode() & 0o777 != 0o700 {
        failure(
            &mut guard,
            "installed dependency type/mode mismatch".to_string(),
        );
    }
    util::atomic_write_json(
        evidence_path,
        &json!({
            "operator": "test-driver",
            "phase": "complete",
            "argv": ["/usr/bin/install", "-m", "0700", source, destination],
            "cwd": checkout,
            "exit": status.code(),
            "sha256": util::pyjson::sha256_hex(&bytes),
            "mode": "0700",
            "path": destination
        }),
    )
    .unwrap();
    destination
}

fn bounded_restore_tool_evidence(mut evidence: J) -> J {
    while serde_json::to_vec_pretty(&evidence).unwrap().len() > 60 * 1024 {
        evidence["commands"].as_array_mut().unwrap().remove(0);
        evidence["truncated"] = json!(true);
    }
    evidence
}

#[test]
fn restored_tool_evidence_retains_recent_records_with_escaped_byte_bound() {
    let records: Vec<_> = (0..8)
        .map(|id| {
            json!({
                "id":id, "command":"\u{0001}".repeat(512),
                "output":"\u{0002}".repeat(1024), "error":"界".repeat(256)
            })
        })
        .collect();
    let evidence =
        bounded_restore_tool_evidence(json!({"commands":records,"total":8,"truncated":false}));
    let bytes = serde_json::to_vec_pretty(&evidence).unwrap();
    assert!(bytes.len() <= 60 * 1024);
    assert_eq!(evidence["total"], 8);
    assert_eq!(evidence["truncated"], true);
    assert_eq!(
        evidence["commands"].as_array().unwrap().last().unwrap()["id"],
        7
    );
    assert_eq!(serde_json::from_slice::<J>(&bytes).unwrap(), evidence);
}

fn restored_review_stage(spec: RestoredReviewSpec<'_>) {
    let RestoredReviewSpec {
        env,
        q,
        guard,
        coordinator,
        unable,
        artifact,
        reviewer_root,
        client,
        endpoint,
        cwd,
        evidence,
    } = spec;
    let artifact_id = artifact["id"].as_str().expect("artifact id");
    let phase1_version = unable["tasks"]["project"]["state_version"].clone();
    let phase1_requirement = unable["tasks"]["project"]["requirements"]["1"].clone();
    let phase1_verdict = unable["tasks"]["accept-native"]["verdict"].clone();
    let phase1_report_path = env
        .workspace_root
        .join("reviewer-checkout/.herdr-verdict.json");
    let phase1_report_bytes = fs::read(&phase1_report_path).unwrap();
    let phase1_pending = unable["tasks"]["project"]["pending"].clone();
    let fresh_cwd = acceptance_live::clone_snapshot(env, artifact, "restored-reviewer-checkout");
    let install_evidence = q.parent().unwrap().join("restore-dependency-install.json");
    let dependency_path = install_operator_dependency(env, &fresh_cwd, &install_evidence);
    let dependency_before = fs::metadata(&dependency_path).unwrap();
    let dependency_hash = util::pyjson::sha256_hex(&fs::read(&dependency_path).unwrap());
    let previous_submission = util::read_json(&guard.join("submission.json")).unwrap();
    queue::signal(
        q,
        "restore-dependency-install",
        "complete",
        "operator dependency installed with actual result evidence",
        Some(install_evidence.to_str().unwrap()),
    )
    .unwrap();

    let deadline = Instant::now() + Duration::from_secs(180);
    let state = loop {
        let state = queue::load_queue(q).unwrap();
        let install = state["events"].as_object().and_then(|events| {
            events
                .values()
                .find(|event| event["wait"] == "restore-dependency-install")
        });
        if install.is_some_and(|event| event["acknowledged"] == true)
            && state["tasks"].get("accept-restored").is_some()
            && util::read_json(&guard.join("submission.json")).is_ok_and(|submission| {
                submission["phase"] == "complete"
                    && submission["message_id"] != previous_submission["message_id"]
            })
        {
            break state;
        }
        assert!(
            Instant::now() < deadline,
            "install signal/continue deadline"
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    let (install_id, install_event) = state["events"]
        .as_object()
        .unwrap()
        .iter()
        .find(|(_, event)| event["wait"] == "restore-dependency-install")
        .map(|(id, event)| (id.clone(), event.clone()))
        .expect("install event");
    assert_eq!(install_event["acknowledged"], true);
    assert_ne!(install_event["applied"], true);
    assert!(install_event["binding"].is_null());
    assert!(state["waits"]["restore-dependency-install"]
        .get("assignment")
        .is_none());
    let install_signal = util::read_json(Path::new(
        install_event["evidence"]
            .as_str()
            .expect("install evidence"),
    ))
    .unwrap();
    assert_eq!(install_signal["evidence"], json!(install_evidence));
    let install_data = util::read_json(&install_evidence).unwrap();
    assert_eq!(install_data["phase"], "complete");
    assert_eq!(install_data["exit"], 0);
    assert_eq!(install_data["mode"], "0700");
    assert_eq!(install_data["sha256"], dependency_hash);
    assert_eq!(install_data["path"], json!(dependency_path));
    assert_eq!(install_data["cwd"], json!(fresh_cwd));
    assert_eq!(state["tasks"]["project"]["pending"], phase1_pending);
    assert!(state["tasks"]["project"]["pending"].is_array());
    assert_eq!(
        state["tasks"]["project"]["pending"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
    let continue_spec = util::read_json(&cwd.join("restore-continue.json")).unwrap();
    assert_eq!(continue_spec["kind"], "continue");
    assert_eq!(continue_spec["task"], "project");
    assert_eq!(continue_spec["state_version"], phase1_version);
    assert_eq!(continue_spec["intent"], "accept-native");
    assert_eq!(continue_spec["authorization"], "verify");
    assert_eq!(continue_spec["action"], "review");
    assert_eq!(continue_spec["resolution"], json!(install_evidence));
    assert_eq!(continue_spec["activity"]["role"], "acceptance");
    assert_eq!(continue_spec["activity"]["input"], artifact_id);
    assert_eq!(continue_spec["activity"]["activity"], "accept-restored");
    assert_eq!(
        continue_spec["activity"]["result_wait"]["id"],
        "accept-restored"
    );
    assert_eq!(
        continue_spec["activity"]["result_wait"]["task"],
        "accept-restored"
    );
    assert_eq!(
        continue_spec["activity"]["result_wait"]["assignment"],
        "accept-restored-a2"
    );
    assert_eq!(continue_spec["activity"]["session_epoch"], 2);
    assert_eq!(state["tasks"]["project"]["approved"], J::Null);
    let rows = session_rows(
        client,
        endpoint,
        coordinator["session"].as_str().unwrap(),
        cwd,
    );
    let commands = tool_commands(&rows, coordinator["session"].as_str().unwrap());
    let relevant: Vec<_> = commands
        .iter()
        .filter(|command| {
            command.command.contains("queue task-lineage")
                && exact_path_argument(&command.command, "--state-dir", q)
        })
        .collect();
    let mut bounded_commands: Vec<_> = relevant
        .iter()
        .rev()
        .take(8)
        .map(|command| {
            json!({
                "command": command.command.chars().take(512).collect::<String>(),
                "status": command.status, "exit": command.exit, "success": command.success,
                "output": command.output.chars().take(1024).collect::<String>(),
                "error": command.error.chars().take(256).collect::<String>(),
                "start": command.start, "end": command.end
            })
        })
        .collect();
    bounded_commands.reverse();
    let tool_evidence = json!({"commands": bounded_commands, "total": relevant.len(),
        "truncated": relevant.len()>8, "field_character_limits": [512, 1024, 256]});
    let tool_evidence = bounded_restore_tool_evidence(tool_evidence);
    util::atomic_write_json(
        &q.parent().unwrap().join("restore-tool-evidence.json"),
        &tool_evidence,
    )
    .unwrap();
    let install_delivery = delivery_for_event(
        &state,
        coordinator["session"].as_str().unwrap(),
        &install_id,
    )
    .expect("install delivery")
    .0
    .to_string();
    let ack = commands
        .iter()
        .position(|command| {
            command.success
                && command.command.contains("queue ack")
                && exact_path_argument(&command.command, "--delivery", Path::new(&install_delivery))
                && exact_path_argument(&command.command, "--state-dir", q)
        })
        .expect("install ACK command");
    let successful_continues: Vec<_> = commands
        .iter()
        .filter(|command| {
            command.success
                && command.command.contains("queue task-lineage")
                && exact_path_argument(&command.command, "--state-dir", q)
        })
        .collect();
    assert_eq!(successful_continues.len(), 1, "one successful continue");
    assert!(exact_path_argument(
        &successful_continues[0].command,
        "--spec",
        &cwd.join("restore-continue.json")
    ));
    let continue_index = commands
        .iter()
        .position(|command| std::ptr::eq(command, successful_continues[0]))
        .unwrap();
    for command in [&commands[ack], &commands[continue_index]] {
        assert!(command.start > 0 && command.start != i64::MAX);
        assert!(command.end >= command.start && command.end != i64::MAX);
    }
    assert!(commands[ack].end <= commands[continue_index].start);
    for command in commands.iter().filter(|command| {
        !command.success
            && command.command.contains("queue task-lineage")
            && exact_path_argument(&command.command, "--state-dir", q)
    }) {
        assert!(
            lock_busy_evidence(command),
            "non-busy continue rejection exit={:?}; see bounded restore-tool-evidence.json",
            command.exit
        );
    }
    let restored = &state["tasks"]["accept-restored"];
    assert_eq!(restored["candidate"], artifact_id);
    assert_eq!(restored["lineage"]["input"], artifact_id);
    assert_eq!(
        restored["lineage"]["round"],
        unable["tasks"]["accept-native"]["lineage"]["round"]
    );
    assert_eq!(
        restored["lineage"]["revision"],
        unable["tasks"]["accept-native"]["lineage"]["revision"]
    );
    assert_eq!(restored["phase"], "waiting");
    assert_eq!(
        state["tasks"]["project"]["continuation"]["phase"],
        "registered"
    );
    assert_eq!(
        restored["acceptance_origin"],
        unable["tasks"]["project"]["continuation"]
    );
    assert_eq!(
        state["tasks"]["project"]["requirements"]["1"],
        phase1_requirement
    );
    assert_eq!(state["tasks"]["project"]["candidate"], artifact_id);
    assert!(state["tasks"].get("repair").is_none());

    native_review_stage(
        env,
        q,
        guard,
        coordinator,
        ReviewSpec {
            artifact,
            reviewer_root,
            activity: "accept-restored",
            checkout: "restored-reviewer-checkout",
            instruction: "You are the independent acceptance reviewer, not the coordinator or developer. Independently inspect the fixed artifact against the original requirement. The operator-provided dependency is now installed. Run `./required-dependency-fixture --check` and `git show HEAD:file` in separate direct bash invocations; do not append echo/status printing, pipes, shell wrappers, or OR-true; preserve each native exit status and use their actual outputs. Include a passed goal check whose source is exactly: Operator-provided acceptance validation requires executable ./required-dependency-fixture --check; it is not part of the developer artifact. Include the exact dependency command and its actual output in that check's evidence. Do not install or modify the dependency, do not commit, and do not ACK or apply anything. Write .herdr-verdict.json with actual passed/failed evidence and derive the conclusion yourself. Do not claim delivery.",
            expected_waits: &["review", "accept-native", "restore-dependency-install", "accept-restored"],
            expect_pass: true,
            unavailable_dependency: None,
            development_session: unable["tasks"]["accept-native"]["verdict"]["envelope"]
                ["reviewer"]["session"]
                .as_str(),
            prepared_checkout: Some(&fresh_cwd),
            capture_tools: true,
            expect_ambiguous: false,
            expected_application: None,
            coordinator_client: None,
            coordinator_endpoint: None,
            coordinator_cwd: None,
        },
    );
    let final_state = queue::load_queue(q).unwrap();
    assert_eq!(final_state["events"].as_object().unwrap().len(), 4);
    assert_eq!(final_state["deliveries"].as_object().unwrap().len(), 4);
    let install = final_state["events"]
        .as_object()
        .unwrap()
        .values()
        .find(|event| event["wait"] == "restore-dependency-install")
        .unwrap();
    assert_eq!(install["acknowledged"], true);
    assert_ne!(install["applied"], true);
    assert_eq!(final_state["tasks"]["project"]["pending"], json!([]));
    assert_eq!(
        final_state["tasks"]["project"]["requirements"]["1"],
        phase1_requirement
    );
    assert_eq!(final_state["tasks"]["project"]["candidate"], artifact_id);
    assert_eq!(
        final_state["tasks"]["project"]["approved"]["candidate"],
        artifact_id
    );
    assert_eq!(
        final_state["tasks"]["project"]["approved"]["activity"],
        "accept-restored"
    );
    let final_report = &final_state["tasks"]["accept-restored"]["verdict"]["envelope"]["report"];
    let dependency_check = final_report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| {
            check["criterion"] == "goal" && check["source"] == UNAVAILABLE_DEPENDENCY_SOURCE
        })
        .expect("fresh report dependency check");
    assert_eq!(dependency_check["status"], "passed");
    assert!(dependency_check["evidence"]
        .as_str()
        .is_some_and(
            |value| value.contains("./required-dependency-fixture --check")
                && value.contains("operator-dependency-v1")
        ));
    let fresh_reviewer = final_state["tasks"]["accept-restored"]["verdict"]["envelope"]["reviewer"]
        ["session"]
        .as_str();
    let phase1_reviewer =
        unable["tasks"]["accept-native"]["verdict"]["envelope"]["reviewer"]["session"].as_str();
    let phase1_worker = unable["waits"]["review"]["producer"]["session"].as_str();
    assert!(fresh_reviewer.is_some());
    assert_ne!(fresh_reviewer, phase1_reviewer);
    assert_ne!(fresh_reviewer, phase1_worker);
    assert_eq!(
        final_state["tasks"]["project"]["continuation"]["action"]["kind"],
        "delivery"
    );
    assert_eq!(
        final_state["tasks"]["accept-native"]["verdict"],
        phase1_verdict
    );
    assert_eq!(fs::read(&phase1_report_path).unwrap(), phase1_report_bytes);
    let dependency_after = fs::metadata(&dependency_path).unwrap();
    assert_eq!(dependency_after.dev(), dependency_before.dev());
    assert_eq!(dependency_after.ino(), dependency_before.ino());
    assert_eq!(dependency_after.mode(), dependency_before.mode());
    assert_eq!(
        util::pyjson::sha256_hex(&fs::read(&dependency_path).unwrap()),
        dependency_hash
    );
    let task_names: std::collections::BTreeSet<_> = final_state["tasks"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        task_names,
        ["project", "review", "accept-native", "accept-restored"]
            .into_iter()
            .collect()
    );
    assert!(final_state["tasks"].get("repair").is_none());
    let tree = git(&fresh_cwd, &["ls-tree", "-r", "--name-only", "HEAD"]);
    assert!(!tree.lines().any(|line| line == UNAVAILABLE_DEPENDENCY));
    evidence.0.push(install_evidence);
    println!(
        "real OpenCode restored review: unable verdict -> install {} -> continue -> fresh pass; no delivery",
        dependency_hash
    );
}

fn assert_target_untouched(
    target: &Path,
    user_asset: &Path,
    user_identity: (u64, u64),
    expected: &str,
) {
    use std::os::unix::fs::MetadataExt;
    assert_eq!(
        git(target, &["rev-parse", "refs/heads/main"]).as_str(),
        expected
    );
    let metadata = fs::metadata(user_asset).unwrap();
    assert_eq!((metadata.dev(), metadata.ino()), user_identity);
    assert_eq!(
        fs::read_to_string(user_asset).unwrap(),
        "unrelated user asset"
    );
}

fn confirm_delivery(
    env: &mut TaskEnvironment,
    q: &Path,
    target: &Path,
    user_asset: &Path,
    artifact: &J,
    expected: &str,
    user_identity: (u64, u64),
) -> J {
    assert_target_untouched(target, user_asset, user_identity, expected);
    let artifact_id = artifact["id"].as_str().unwrap();
    let state = queue::load_queue(q).unwrap();
    let mut spec = json!({
        "adapter":"git",
        "target":target.display().to_string(),
        "ref":"refs/heads/main",
        "expected":expected,
        "authorization":"deliver"
    });
    spec["state_version"] = state["tasks"]["project"]["state_version"].clone();
    let intent = delivery::prepare(env, q, &spec, &delivery::local::Local).unwrap();
    assert_eq!(intent["artifact"]["id"], json!(artifact_id));
    let intent_id = intent["id"].clone();
    assert_target_untouched(target, user_asset, user_identity, expected);
    let confirmed = delivery::execute(env, &delivery::local::Local).unwrap();
    assert_eq!(confirmed["phase"], "confirmed");
    assert_eq!(confirmed["id"], intent_id);
    assert_eq!(
        confirmed["receipt"]["repository"],
        json!(target.display().to_string())
    );
    assert_eq!(confirmed["receipt"]["ref"], "refs/heads/main");
    assert_eq!(confirmed["receipt"]["commit"], json!(artifact_id));
    assert_eq!(
        git(target, &["rev-parse", "refs/heads/main"]).as_str(),
        artifact_id
    );
    assert_eq!(
        fs::read_to_string(user_asset).unwrap(),
        "unrelated user asset"
    );
    confirmed
}

fn cli_output(args: &[&str]) -> std::process::Output {
    use std::io::{Read, Seek, SeekFrom};
    fn read_bounded(file: &mut std::fs::File) -> Vec<u8> {
        let mut bytes = Vec::new();
        let _ = file.seek(SeekFrom::Start(0));
        let _ = file.take(1024 * 1024).read_to_end(&mut bytes);
        bytes
    }
    let mut stdout_file = tempfile::tempfile().unwrap();
    let mut stderr_file = tempfile::tempfile().unwrap();
    let mut child = CliChild {
        child: Some(
            std::process::Command::new(env!("CARGO_BIN_EXE_herdr-flow"))
                .args(args)
                .stdout(std::process::Stdio::from(stdout_file.try_clone().unwrap()))
                .stderr(std::process::Stdio::from(stderr_file.try_clone().unwrap()))
                .spawn()
                .unwrap(),
        ),
    };
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let status = child.child.as_mut().unwrap().try_wait().unwrap();
        if let Some(status) = status {
            let _ = child.child.take();
            return std::process::Output {
                status,
                stdout: read_bounded(&mut stdout_file),
                stderr: read_bounded(&mut stderr_file),
            };
        }
        if Instant::now() >= deadline {
            if let Some(mut process) = child.child.take() {
                let _ = process.kill();
                let _ = process.wait();
            }
            panic!(
                "delivery CLI timed out; stdout={:?}; stderr={:?}",
                String::from_utf8_lossy(&read_bounded(&mut stdout_file)),
                String::from_utf8_lossy(&read_bounded(&mut stderr_file))
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

struct CliChild {
    child: Option<std::process::Child>,
}
impl Drop for CliChild {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn wait_bounded(child: &mut std::process::Child, label: &str) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Ok(None) => {
                reap_after_kill(child, label);
                panic!("{label} did not exit within 15s");
            }
            Err(error) => {
                reap_after_kill(child, label);
                panic!("{label} wait failed: {error}");
            }
        }
    }
}

fn reap_after_kill(child: &mut std::process::Child, label: &str) {
    let _ = child.kill();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Ok(None) => {
                eprintln!("{label} could not be reaped within 5s after kill");
                return;
            }
            Err(error) => {
                eprintln!("{label} reap failed: {error}");
                return;
            }
        }
    }
}

struct ChildReaper<'a> {
    server: &'a mut std::process::Child,
    guard: &'a mut std::process::Child,
    armed: bool,
}
impl<'a> ChildReaper<'a> {
    fn new(server: &'a mut std::process::Child, guard: &'a mut std::process::Child) -> Self {
        Self {
            server,
            guard,
            armed: true,
        }
    }
    fn kill(&mut self) {
        if !self.armed {
            return;
        }
        self.armed = false;
        reap_after_kill(self.server, "native server");
        reap_after_kill(self.guard, "guard watcher");
    }
    fn wait(mut self) {
        wait_bounded(self.server, "native server");
        wait_bounded(self.guard, "guard watcher");
        self.armed = false;
    }
}
impl Drop for ChildReaper<'_> {
    fn drop(&mut self) {
        self.kill();
    }
}

struct RestartSpec<'a> {
    inventory: super::native_inventory::NativeInventory,
    target: &'a Path,
    user_asset: &'a Path,
    artifact: &'a J,
    expected: &'a str,
    user_identity: (u64, u64),
}

fn external_supervisor_restart(
    mut env: super::OwnedEnvironment,
    q: &Path,
    server: &mut std::process::Child,
    guard_child: &mut std::process::Child,
    spec: RestartSpec<'_>,
) {
    let RestartSpec {
        inventory,
        target,
        user_asset,
        artifact,
        expected,
        user_identity,
    } = spec;
    let workspace = env.workspace_root.clone();
    let temporary = env.temp_root.clone();
    let state_file = env.state_file().to_path_buf();
    let mut custody;
    let mut reaper = ChildReaper::new(server, guard_child);
    let confirmed = confirm_delivery(
        &mut env,
        q,
        target,
        user_asset,
        artifact,
        expected,
        user_identity,
    );
    let artifact_id = artifact["id"].as_str().unwrap();
    let intent_id = confirmed["id"].clone();
    let receipt = confirmed["receipt"].clone();
    let expected_inventory = inventory.before(&confirmed);
    custody = env.into_handoff();

    let reconcile = cli_output(&["delivery", "reconcile", "--task", "project"]);
    if !reconcile.status.success() {
        reaper.kill();
        custody.cleanup();
        panic!(
            "fresh reconcile failed: {}",
            String::from_utf8_lossy(&reconcile.stderr)
        );
    }
    let reconciled: J = serde_json::from_slice(&reconcile.stdout)
        .unwrap_or_else(|error| panic!("reconcile output invalid: {error}"));
    assert_eq!(reconciled["phase"], "confirmed");
    assert_eq!(reconciled["id"], intent_id);
    assert_eq!(reconciled["receipt"], receipt);
    assert_target_untouched(target, user_asset, user_identity, artifact_id);

    let finish = cli_output(&["delivery", "finish", "--task", "project"]);
    if !finish.status.success() {
        reaper.kill();
        custody.cleanup();
        panic!(
            "fresh finish failed: {}",
            String::from_utf8_lossy(&finish.stderr)
        );
    }
    reaper.wait();
    assert!(!workspace.exists());
    assert!(!temporary.exists());
    assert!(!state_file.exists());
    assert!(!is_registered("project").unwrap());
    assert!(!cleanup_state_path("project").unwrap().exists());
    assert_eq!(occupied_slots().unwrap(), 0);
    assert!(!q.exists());
    assert_target_untouched(target, user_asset, user_identity, artifact_id);
    assert_eq!(git(target, &["show", "refs/heads/main:file"]), "version B");
    inventory.after(&expected_inventory);
    custody.disarm();
}

struct LostObservation {
    executions: AtomicUsize,
}

impl Default for LostObservation {
    fn default() -> Self {
        Self {
            executions: AtomicUsize::new(0),
        }
    }
}

impl Executor for LostObservation {
    fn preflight(&self, intent: &J) -> herdr_flow::Result<()> {
        delivery::local::Local.preflight(intent)
    }

    fn execute(&self, intent: &J) -> herdr_flow::Result<()> {
        if self
            .executions
            .compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Err(herdr_flow::FlowError::progress(
                "test executor already performed its single Local mutation",
            ));
        }
        delivery::local::Local.execute(intent)
    }

    fn observe(&self, _: &J) -> herdr_flow::Result<Observation> {
        Ok(Observation::Unknown(
            "test-injected lost observation after one Local execution".into(),
        ))
    }
}

#[test]
fn lost_observation_allows_one_execution_attempt_and_never_confirms() {
    let executor = LostObservation::default();
    let invalid = json!({});
    assert!(executor.execute(&invalid).is_err());
    assert_eq!(executor.executions.load(Ordering::SeqCst), 1);
    assert!(executor.execute(&invalid).is_err());
    assert_eq!(executor.executions.load(Ordering::SeqCst), 1);
    assert!(matches!(
        executor.observe(&invalid),
        Ok(Observation::Unknown(_))
    ));
}

// Trusted, unmarked external driver. The native coordinator only approved the
// continuation; this process owns the environment and the Local executor.
struct DeliverySpec<'a> {
    q: &'a Path,
    target: &'a Path,
    user_asset: &'a Path,
    artifact: &'a J,
    expected: &'a str,
    user_identity: (u64, u64),
    inventory: super::native_inventory::NativeInventory,
}

fn bounded_delivery_evidence(value: &J) -> J {
    let bytes = serde_json::to_vec_pretty(value).unwrap();
    if bytes.len() <= 60 * 1024 {
        return value.clone();
    }
    json!({"truncated": true, "original_bytes": bytes.len(),
        "reason": "delivery diagnostic exceeded 60KiB; payload omitted"})
}

#[test]
fn delivery_evidence_caps_escaped_and_multibyte_payloads() {
    let small = json!({"phase":"unknown"});
    assert_eq!(bounded_delivery_evidence(&small), small);
    let large = json!({"data": "\u{0001}界".repeat(20000)});
    let bounded = bounded_delivery_evidence(&large);
    assert_eq!(bounded["truncated"], true);
    assert!(bounded.get("data").is_none());
    assert!(serde_json::to_vec_pretty(&bounded).unwrap().len() <= 60 * 1024);
}

fn external_unknown_delivery(
    mut env: super::OwnedEnvironment,
    q: &Path,
    server: &mut std::process::Child,
    guard_child: &mut std::process::Child,
    spec: DeliverySpec<'_>,
    unknown_evidence: &Path,
    confirmation_evidence: &Path,
) {
    let DeliverySpec {
        q: delivery_queue,
        target,
        user_asset,
        artifact,
        expected,
        user_identity,
        inventory,
    } = spec;
    assert_eq!(delivery_queue, q);
    let workspace = env.workspace_root.clone();
    let temporary = env.temp_root.clone();
    let state_file = env.state_file().to_path_buf();
    let artifact_id = artifact["id"].as_str().unwrap();
    let user_before = fs::read(user_asset).unwrap();
    let m = fs::metadata(user_asset).unwrap();
    assert_eq!((m.dev(), m.ino()), user_identity);
    assert_eq!(git(target, &["rev-parse", "refs/heads/main"]), expected);
    let state = queue::load_queue(q).unwrap();
    let mut request = json!({
        "adapter":"git",
        "target":target.display().to_string(),
        "ref":"refs/heads/main",
        "expected":expected,
        "authorization":"deliver"
    });
    request["state_version"] = state["tasks"]["project"]["state_version"].clone();

    let mut custody;
    let mut reaper = ChildReaper::new(server, guard_child);
    let intent = delivery::prepare(&mut env, q, &request, &delivery::local::Local).unwrap();
    let intent_id = intent["id"].clone();
    assert_eq!(intent["artifact"]["id"], artifact_id);
    assert_eq!(git(target, &["rev-parse", "refs/heads/main"]), expected);
    assert_eq!(fs::read(user_asset).unwrap(), user_before);
    let executor = LostObservation::default();
    let unknown = delivery::execute(&mut env, &executor).unwrap();
    custody = env.into_handoff();
    use std::io::Read;
    let mut state_bytes = Vec::new();
    fs::File::open(&state_file)
        .unwrap()
        .take(1024 * 1024 + 1)
        .read_to_end(&mut state_bytes)
        .unwrap();
    assert!(
        state_bytes.len() <= 1024 * 1024,
        "environment state exceeds 1MiB"
    );
    let persisted_document: J = serde_json::from_slice(&state_bytes).unwrap();
    let persisted = &persisted_document["delivery"];
    util::atomic_write_json(
        unknown_evidence,
        &bounded_delivery_evidence(&json!({
            "returned": {
                "phase": unknown["phase"], "id": unknown["id"],
                "request": unknown["request"], "artifact": unknown["artifact"],
                "receipt": unknown["receipt"], "has_problem": unknown["problem"].is_string(),
                "has_execution_error": unknown["execution_error"].is_string(),
            },
            "persisted": {
                "phase": persisted["phase"], "id": persisted["id"],
                "request": persisted["request"], "artifact": persisted["artifact"],
                "receipt": persisted["receipt"], "has_problem": persisted["problem"].is_string(),
                "has_execution_error": persisted["execution_error"].is_string(),
            }
        })),
    )
    .unwrap();
    assert_eq!(executor.executions.load(Ordering::SeqCst), 1);
    assert_eq!(persisted["phase"], unknown["phase"]);
    assert_eq!(persisted["id"], unknown["id"]);
    assert_eq!(persisted["request"], unknown["request"]);
    assert_eq!(persisted["artifact"], unknown["artifact"]);
    assert!(persisted["receipt"].is_null());
    assert_eq!(unknown["phase"], "unknown");
    assert!(unknown["receipt"].is_null());
    assert_eq!(unknown["id"], intent_id);
    assert_eq!(unknown["artifact"]["id"], artifact_id);
    assert_eq!(unknown["request"], request);
    assert_eq!(git(target, &["rev-parse", "refs/heads/main"]), artifact_id);
    assert_eq!(fs::read(user_asset).unwrap(), user_before);
    let m = fs::metadata(user_asset).unwrap();
    assert_eq!((m.dev(), m.ino()), user_identity);
    let expected_unknown = inventory.before_unknown(&unknown);

    let reconcile = cli_output(&["delivery", "reconcile", "--task", "project"]);
    if !reconcile.status.success() {
        reaper.kill();
        custody.cleanup();
        panic!(
            "fresh unknown reconcile failed: {}",
            String::from_utf8_lossy(&reconcile.stderr)
        );
    }
    let confirmed: J = serde_json::from_slice(&reconcile.stdout)
        .unwrap_or_else(|error| panic!("unknown reconcile output invalid: {error}"));
    util::atomic_write_json(
        confirmation_evidence,
        &bounded_delivery_evidence(&json!({
            "pre": {
                "candidate": expected_unknown["candidate"],
                "intent": expected_unknown["intent"],
                "receipt": expected_unknown["receipt"],
                "queue_keys": expected_unknown["queue_keys"],
            },
            "confirmed": {
                "id": confirmed["id"], "artifact": confirmed["artifact"],
                "request": confirmed["request"], "receipt": confirmed["receipt"],
            }
        })),
    )
    .unwrap();
    assert_eq!(confirmed["phase"], "confirmed");
    assert_eq!(confirmed["id"], expected_unknown["intent"]);
    assert_eq!(confirmed["artifact"]["id"], expected_unknown["candidate"]);
    assert_eq!(confirmed["id"], intent_id);
    assert_eq!(confirmed["artifact"]["id"], artifact_id);
    assert_eq!(confirmed["request"], request);
    assert_eq!(confirmed["receipt"]["commit"], artifact_id);
    assert_eq!(
        confirmed["receipt"]["repository"],
        json!(target.display().to_string())
    );
    assert_eq!(confirmed["receipt"]["ref"], "refs/heads/main");
    assert_eq!(git(target, &["rev-parse", "refs/heads/main"]), artifact_id);
    assert_eq!(fs::read(user_asset).unwrap(), user_before);
    let m = fs::metadata(user_asset).unwrap();
    assert_eq!((m.dev(), m.ino()), user_identity);

    let finish = cli_output(&["delivery", "finish", "--task", "project"]);
    if !finish.status.success() {
        reaper.kill();
        custody.cleanup();
        panic!(
            "fresh unknown finish failed: {}",
            String::from_utf8_lossy(&finish.stderr)
        );
    }
    reaper.wait();
    assert_eq!(executor.executions.load(Ordering::SeqCst), 1);
    assert!(!workspace.exists());
    assert!(!temporary.exists());
    assert!(!state_file.exists());
    assert!(!is_registered("project").unwrap());
    assert!(!cleanup_state_path("project").unwrap().exists());
    assert_eq!(occupied_slots().unwrap(), 0);
    assert!(!q.exists());
    assert_eq!(git(target, &["rev-parse", "refs/heads/main"]), artifact_id);
    assert_eq!(git(target, &["show", "refs/heads/main:file"]), "version B");
    assert_eq!(fs::read(user_asset).unwrap(), user_before);
    let m = fs::metadata(user_asset).unwrap();
    assert_eq!((m.dev(), m.ino()), user_identity);
    inventory.after(&expected_unknown);
    custody.disarm();
}

fn external_driver_delivery(
    env: &mut TaskEnvironment,
    spec: DeliverySpec<'_>,
) -> (super::native_inventory::NativeInventory, J) {
    let DeliverySpec {
        q,
        target,
        user_asset,
        artifact,
        expected,
        user_identity,
        inventory,
    } = spec;
    use std::os::unix::fs::MetadataExt;
    let m = fs::metadata(user_asset).unwrap();
    assert_eq!((m.dev(), m.ino()), user_identity);
    let artifact_id = artifact["id"].as_str().unwrap();
    assert_eq!(
        git(target, &["rev-parse", "refs/heads/main"]).as_str(),
        expected
    );
    assert_eq!(
        fs::read_to_string(user_asset).unwrap(),
        "unrelated user asset"
    );
    let state = queue::load_queue(q).unwrap();
    let mut spec = json!({
        "adapter":"git",
        "target":target.display().to_string(),
        "ref":"refs/heads/main",
        "expected":expected,
        "authorization":"deliver"
    });
    assert_eq!(
        state["tasks"]["project"]["requirements"]["1"]["basis"]["endpoint"],
        json!(target)
    );
    spec["state_version"] = state["tasks"]["project"]["state_version"].clone();
    let intent = delivery::prepare(env, q, &spec, &delivery::local::Local).unwrap();
    assert_eq!(intent["artifact"]["id"], json!(artifact_id));
    // Preparation is not delivery: the exact seeded target must be unchanged.
    assert_eq!(
        git(target, &["rev-parse", "refs/heads/main"]).as_str(),
        expected
    );
    assert_eq!(
        fs::read_to_string(user_asset).unwrap(),
        "unrelated user asset"
    );

    let confirmed = delivery::execute(env, &delivery::local::Local).unwrap();
    assert_eq!(confirmed["phase"], "confirmed");
    assert_eq!(
        confirmed["receipt"]["repository"],
        json!(target.display().to_string())
    );
    assert_eq!(confirmed["receipt"]["ref"], "refs/heads/main");
    assert_eq!(confirmed["receipt"]["commit"], json!(artifact_id));
    assert_eq!(
        git(target, &["rev-parse", "refs/heads/main"]).as_str(),
        artifact_id
    );
    assert_eq!(
        fs::read_to_string(user_asset).unwrap(),
        "unrelated user asset"
    );
    let expected_inventory = inventory.before(&confirmed);

    delivery::finish(env).unwrap();
    let m = fs::metadata(user_asset).unwrap();
    assert_eq!((m.dev(), m.ino()), user_identity);
    assert_eq!(git(target, &["show", "refs/heads/main:file"]), "version B");
    assert!(!env.workspace_root.exists());
    assert!(!env.temp_root.exists());
    assert!(!env.state_file().exists());
    assert!(!is_registered("project").unwrap());
    assert!(!cleanup_state_path("project").unwrap().exists());
    assert_eq!(occupied_slots().unwrap(), 0);
    assert!(!q.exists());
    assert_eq!(
        git(target, &["rev-parse", "refs/heads/main"]).as_str(),
        artifact_id
    );
    assert_eq!(
        fs::read_to_string(user_asset).unwrap(),
        "unrelated user asset"
    );
    (inventory, expected_inventory)
}

fn wait_for_stage(q: &Path, guard: &Path, coordinator: &J, wait: &str) -> J {
    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        let state = queue::load_queue(q).unwrap();
        let matches: Vec<_> = state["events"]
            .as_object()
            .unwrap()
            .iter()
            .filter(|(_, event)| event["wait"] == wait)
            .collect();
        assert!(matches.len() <= 1, "duplicate {wait} event");
        if let Some((id, event)) = matches.first() {
            let receipt = state["deliveries"]
                .as_object()
                .unwrap()
                .iter()
                .find(|(_, delivery)| {
                    delivery["events"] == json!([id])
                        && delivery["phase"] == "acknowledged"
                        && delivery["session"] == coordinator["session"]
                });
            if let Some((delivery, _)) = receipt {
                let readiness = util::read_json(&q.join("readiness.json")).unwrap_or(J::Null);
                let submission = util::read_json(&guard.join("submission.json")).unwrap_or(J::Null);
                assert_ne!(
                    submission["native_status"], "blocked",
                    "native approval blocked {wait}"
                );
                if event["applied"] == true
                    && readiness["active_wakeup"] == true
                    && readiness["last_confirmed_receipt"]["delivery"] == *delivery
                    && readiness["last_confirmed_receipt"]["session"] == coordinator["session"]
                    && submission["phase"] == "complete"
                {
                    return state;
                }
            }
        }
        assert!(
            Instant::now() < deadline,
            "native {wait} application deadline"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn assert_stage_shape(state: &J, waits: &[&str], coordinator: &J) {
    assert_eq!(state["events"].as_object().unwrap().len(), waits.len());
    assert_eq!(state["deliveries"].as_object().unwrap().len(), waits.len());
    for wait in waits {
        let matches: Vec<_> = state["events"]
            .as_object()
            .unwrap()
            .iter()
            .filter(|(_, event)| event["wait"] == *wait)
            .collect();
        assert_eq!(matches.len(), 1, "exactly one event for {wait}");
        let (id, event) = matches[0];
        assert_eq!(event["acknowledged"], true);
        assert_eq!(event["applied"], true);
        let deliveries: Vec<_> = state["deliveries"]
            .as_object()
            .unwrap()
            .values()
            .filter(|delivery| delivery["events"] == json!([id]))
            .collect();
        assert_eq!(deliveries.len(), 1, "exactly one delivery for {wait}");
        assert_eq!(deliveries[0]["session"], coordinator["session"]);
        assert_eq!(deliveries[0]["phase"], "acknowledged");
    }
}

fn assert_restored_shape(state: &J, waits: &[&str], coordinator: &J) {
    assert_eq!(state["events"].as_object().unwrap().len(), 4);
    assert_eq!(state["deliveries"].as_object().unwrap().len(), 4);
    for wait in waits {
        let matches: Vec<_> = state["events"]
            .as_object()
            .unwrap()
            .iter()
            .filter(|(_, event)| event["wait"] == *wait)
            .collect();
        assert_eq!(matches.len(), 1, "exactly one event for {wait}");
        let (id, event) = matches[0];
        assert_eq!(event["acknowledged"], true);
        if *wait == "restore-dependency-install" {
            assert_ne!(event["applied"], true);
            assert!(event["binding"].is_null());
        } else {
            assert_eq!(event["applied"], true);
        }
        let deliveries: Vec<_> = state["deliveries"]
            .as_object()
            .unwrap()
            .values()
            .filter(|delivery| delivery["events"] == json!([id]))
            .collect();
        assert_eq!(deliveries.len(), 1, "exactly one delivery for {wait}");
        assert_eq!(deliveries[0]["session"], coordinator["session"]);
        assert_eq!(deliveries[0]["phase"], "acknowledged");
    }
    assert_eq!(state["tasks"]["project"]["pending"], json!([]));
}

fn assert_ambiguous_application_commands(
    q: &Path,
    state: &J,
    coordinator: &J,
    event_id: &str,
    application: &Path,
    commands: &[ToolCommand],
) {
    let session = coordinator["session"]
        .as_str()
        .expect("coordinator session");
    let delivery = delivery_for_event(state, session, event_id).expect("acceptance delivery");
    let ack = commands
        .iter()
        .find(|command| {
            command.success
                && command.command.contains("queue ack")
                && exact_path_argument(&command.command, "--delivery", Path::new(delivery.0))
                && exact_path_argument(&command.command, "--state-dir", q)
        })
        .expect("exact acceptance ACK");
    let applies: Vec<_> = commands
        .iter()
        .filter(|command| {
            command.success
                && command.command.contains("queue task-apply")
                && exact_path_argument(&command.command, "--state-dir", q)
        })
        .collect();
    assert_eq!(applies.len(), 1, "one successful ambiguity application");
    let application_command = applies[0];
    assert!(exact_path_argument(
        &application_command.command,
        "--spec",
        application
    ));
    let readiness = commands
        .iter()
        .find(|command| {
            command.success
                && command.command.contains("queue readiness")
                && exact_path_argument(&command.command, "--state-dir", q)
        })
        .expect("successful ambiguity readiness");
    for command in [ack, application_command, readiness] {
        assert!(command.start > 0 && command.start != i64::MAX);
        assert!(command.end >= command.start && command.end != i64::MAX);
    }
    assert!(ack.end <= application_command.start);
    assert!(application_command.end <= readiness.start);
    for command in commands.iter().filter(|command| {
        !command.success
            && command.command.contains("queue task-apply")
            && exact_path_argument(&command.command, "--state-dir", q)
    }) {
        assert!(
            lock_busy_evidence(command),
            "non-busy ambiguity rejection exit={:?}",
            command.exit
        );
    }
    assert!(
        commands.iter().all(|command| {
            !command.command.contains("queue task-lineage")
                && !command.command.contains("queue task-revise")
        }),
        "ambiguity phase must stop without revision or continuation"
    );
}

fn assert_missing_report_application_commands(
    q: &Path,
    state: &J,
    coordinator: &J,
    event_id: &str,
    application: &Path,
    commands: &[ToolCommand],
) {
    let session = coordinator["session"]
        .as_str()
        .expect("coordinator session");
    let delivery = delivery_for_event(state, session, event_id).expect("acceptance delivery");
    let ack = commands
        .iter()
        .find(|command| {
            command.success
                && command.command.contains("queue ack")
                && exact_path_argument(&command.command, "--delivery", Path::new(delivery.0))
                && exact_path_argument(&command.command, "--state-dir", q)
        })
        .expect("exact missing-report ACK");
    let applies: Vec<_> = commands
        .iter()
        .filter(|command| {
            command.success
                && command.command.contains("queue task-apply")
                && exact_path_argument(&command.command, "--state-dir", q)
        })
        .collect();
    assert_eq!(applies.len(), 1, "one successful missing-report apply");
    let apply = applies[0];
    assert!(exact_path_argument(&apply.command, "--spec", application));
    let readiness = commands
        .iter()
        .find(|command| {
            command.success
                && command.command.contains("queue readiness")
                && exact_path_argument(&command.command, "--state-dir", q)
                && command.start >= apply.end
        })
        .expect("post-apply missing-report readiness");
    for command in [ack, apply, readiness] {
        assert!(command.start > 0 && command.start != i64::MAX);
        assert!(command.end >= command.start && command.end != i64::MAX);
    }
    assert!(ack.end <= apply.start);
    assert!(apply.end <= readiness.start);
    let readiness_doc = util::read_json(&q.join("readiness.json")).unwrap();
    assert_eq!(readiness_doc["active_wakeup"], true);
    assert_eq!(
        readiness_doc["last_confirmed_receipt"]["delivery"],
        delivery.0
    );
    assert_eq!(readiness_doc["last_confirmed_receipt"]["session"], session);
    let readiness_output: J =
        serde_json::from_str(readiness.output.trim()).expect("readiness command JSON output");
    assert_eq!(readiness_output, readiness_doc);
    let checked_at = readiness_doc["checked_at"]
        .as_f64()
        .expect("readiness checked_at");
    assert!(
        checked_at * 1000.0 >= readiness.start as f64
            && checked_at * 1000.0 <= readiness.end as f64 + 1.0,
        "readiness file does not belong to the post-apply invocation"
    );
    for command in commands.iter().filter(|command| {
        !command.success
            && command.command.contains("queue task-apply")
            && exact_path_argument(&command.command, "--state-dir", q)
    }) {
        assert!(
            lock_busy_evidence(command),
            "non-busy missing-report rejection exit={:?}",
            command.exit
        );
    }
    assert!(
        commands.iter().all(|command| {
            !command.command.contains("queue task-lineage")
                && !command.command.contains("queue task-revise")
        }),
        "missing-report phase must not continue or revise"
    );
}

struct ReviewSpec<'a> {
    artifact: &'a J,
    reviewer_root: &'a Path,
    activity: &'a str,
    checkout: &'a str,
    instruction: &'a str,
    expected_waits: &'a [&'a str],
    expect_pass: bool,
    unavailable_dependency: Option<&'a str>,
    development_session: Option<&'a str>,
    prepared_checkout: Option<&'a Path>,
    capture_tools: bool,
    expect_ambiguous: bool,
    expected_application: Option<&'a Path>,
    coordinator_client: Option<&'a ureq::Agent>,
    coordinator_endpoint: Option<&'a str>,
    coordinator_cwd: Option<&'a Path>,
}

fn native_review_stage(
    env: &mut TaskEnvironment,
    q: &Path,
    guard: &Path,
    coordinator: &J,
    spec: ReviewSpec<'_>,
) -> J {
    let ReviewSpec {
        artifact,
        reviewer_root,
        activity: activity_name,
        checkout: checkout_name,
        instruction: reviewer_instruction,
        expected_waits,
        expect_pass,
        unavailable_dependency,
        development_session,
        prepared_checkout,
        capture_tools,
        expect_ambiguous,
        expected_application,
        coordinator_client,
        coordinator_endpoint,
        coordinator_cwd,
    } = spec;
    let parent_before = queue::load_queue(q).unwrap()["tasks"]["project"].clone();
    let requirement_before = parent_before["requirements"]["1"].clone();
    let revision_before = parent_before["requirement_revision"].clone();
    let cwd = if let Some(prepared) = prepared_checkout {
        assert!(
            queue::load_queue(q).unwrap()["tasks"]
                .get(activity_name)
                .is_some(),
            "prepared checkout requires registered activity"
        );
        prepared.to_path_buf()
    } else {
        acceptance_live::change(
            q,
            json!({"kind":"candidate","candidate":artifact["id"],"reason":"real independent acceptance"}),
        );
        let mut activity = acceptance_live::activity(activity_name, "acceptance", &artifact["id"]);
        activity["kind"] = json!("activity");
        acceptance_live::change(q, activity);
        acceptance_live::clone_snapshot(env, artifact, checkout_name)
    };
    let ambiguity_snapshot = expect_ambiguous.then(|| {
        (
            git(&cwd, &["rev-parse", "HEAD"]),
            fs::read(cwd.join("file")).unwrap(),
        )
    });
    let prepared_head = prepared_checkout.map(|_| git(&cwd, &["rev-parse", "HEAD"]));
    let mut reviewer_tool_rows = J::Null;
    let (reviewer, collected) =
        if unavailable_dependency.is_some() || capture_tools || expect_ambiguous {
            if let Some(dependency) = unavailable_dependency {
                let dependency_path = cwd.join(dependency);
                assert!(
                    !dependency_path.exists(),
                    "operator dependency unexpectedly present"
                );
                let tree = git(&cwd, &["ls-tree", "-r", "--name-only", "HEAD"]);
                assert!(
                    !tree.lines().any(|line| line == dependency),
                    "operator dependency must not be committed in artifact A"
                );
            }
            acceptance_live::native_with_hook(
                env,
                q,
                &cwd,
                activity_name,
                reviewer_instruction,
                || {},
                |client, endpoint, result| {
                    let session = result["session"].as_str().expect("reviewer result session");
                    reviewer_tool_rows = session_rows(client, endpoint, session, &cwd);
                },
            )
        } else {
            acceptance_live::native(env, q, &cwd, activity_name, reviewer_instruction)
        };
    assert_ne!(reviewer["session"], coordinator["session"]);
    if let Some(development_session) = development_session {
        assert_ne!(reviewer["session"], development_session);
    }
    let report_path = cwd.join(".herdr-verdict.json");
    let actual_report_bytes = fs::read(&report_path).unwrap();
    let actual_report = util::read_json(&report_path).unwrap();
    let state = wait_for_stage(q, guard, coordinator, activity_name);
    assert_eq!(fs::read(&report_path).unwrap(), actual_report_bytes);
    let coordinator_commands = match (coordinator_client, coordinator_endpoint, coordinator_cwd) {
        (Some(client), Some(endpoint), Some(cwd)) => {
            let rows = session_rows(
                client,
                endpoint,
                coordinator["session"]
                    .as_str()
                    .expect("coordinator session"),
                cwd,
            );
            tool_commands(
                &rows,
                coordinator["session"]
                    .as_str()
                    .expect("coordinator session"),
            )
        }
        _ => Vec::new(),
    };
    if prepared_checkout.is_some() {
        assert_restored_shape(&state, expected_waits, coordinator);
    } else {
        assert_stage_shape(&state, expected_waits, coordinator);
    }
    let task = &state["tasks"][activity_name];
    assert_eq!(task["pending"], json!([]));
    let verdict = &task["verdict"];
    let event = state["events"]
        .as_object()
        .unwrap()
        .values()
        .find(|event| event["wait"] == activity_name)
        .unwrap();
    assert_eq!(event["binding"], collected["context"]["binding"]);
    assert_eq!(verdict["envelope"]["binding"], event["binding"]);
    assert_eq!(
        verdict["envelope"]["reviewer"]["session"],
        reviewer["session"]
    );
    assert_eq!(verdict["envelope"]["reviewer"]["adapter"], "opencode");
    assert_eq!(verdict["envelope"]["report"], actual_report);
    assert_eq!(
        util::read_json(&cwd.join(".herdr-verdict.json")).unwrap(),
        actual_report
    );
    assert_eq!(event["outcome"], "complete");
    assert_eq!(
        event["evidence"],
        json!(q.join(format!("results/{activity_name}.json")))
    );
    let signal = util::read_json(Path::new(event["evidence"].as_str().unwrap())).unwrap();
    assert_eq!(signal["wait"], activity_name);
    assert_eq!(signal["outcome"], "complete");
    assert_eq!(
        signal["evidence"],
        json!(reviewer_root.join("submissions").join(format!(
            "{}.json",
            collected["message_id"].as_str().unwrap()
        )))
    );
    let report = util::read_json(Path::new(signal["evidence"].as_str().unwrap())).unwrap();
    assert_eq!(report["session"], reviewer["session"]);
    if let Some(dependency) = unavailable_dependency {
        assert_eq!(actual_report["conclusion"], "unable");
        let checks = actual_report["checks"]
            .as_array()
            .expect("unable report checks");
        let dependency_check = checks
            .iter()
            .find(|check| {
                check["criterion"] == "goal" && check["source"] == UNAVAILABLE_DEPENDENCY_SOURCE
            })
            .expect("dependency check");
        assert_eq!(dependency_check["status"], "blocked");
        assert_eq!(dependency_check["source"], UNAVAILABLE_DEPENDENCY_SOURCE);
        assert!(dependency_check["evidence"]
            .as_str()
            .is_some_and(|value| value.contains(dependency)));
        assert!(actual_report["blocker"]
            .as_str()
            .is_some_and(|value| { value.contains(dependency) && value.contains("127") }));
        assert!(actual_report["restore_condition"]
            .as_str()
            .is_some_and(|value| value.contains(dependency)));
        let commands = tool_commands(
            &reviewer_tool_rows,
            reviewer["session"].as_str().expect("reviewer session"),
        );
        let command = commands
            .iter()
            .find(|command| command.command.trim() == format!("./{dependency} --check"))
            .expect("missing dependency command evidence");
        assert!(
            terminal_tool_state(command),
            "missing dependency command not terminal"
        );
        assert_eq!(command.exit, Some(127));
        assert_eq!(
            command.command.trim(),
            format!("./{dependency} --check"),
            "reviewer did not execute the exact dependency command"
        );
        let missing_output = command.output.to_ascii_lowercase();
        assert!(
            missing_output.contains("not found")
                || missing_output.contains("no such file")
                || missing_output.contains("cannot access")
        );
        assert!(
            command.start > 0
                && command.start != i64::MAX
                && command.end >= command.start
                && command.end != i64::MAX
        );
        assert!(!cwd.join(dependency).exists());
    }
    if capture_tools {
        let commands = tool_commands(
            &reviewer_tool_rows,
            reviewer["session"].as_str().expect("reviewer session"),
        );
        let dependency = commands
            .iter()
            .find(|command| command.command.trim() == "./required-dependency-fixture --check")
            .expect("fresh dependency check evidence");
        assert!(
            terminal_tool_state(dependency),
            "fresh dependency check not terminal"
        );
        assert_eq!(dependency.exit, Some(0));
        assert!(dependency.output.contains("operator-dependency-v1"));
        assert!(
            dependency.start > 0
                && dependency.start != i64::MAX
                && dependency.end >= dependency.start
                && dependency.end != i64::MAX
        );
        let product = commands
            .iter()
            .find(|command| command.command.trim() == "git show HEAD:file")
            .expect("fresh product check evidence");
        assert!(
            terminal_tool_state(product),
            "fresh product check not terminal"
        );
        assert_eq!(product.exit, Some(0));
        assert_eq!(product.output.trim(), "version B");
        assert!(
            product.start > 0
                && product.start != i64::MAX
                && product.end >= product.start
                && product.end != i64::MAX
        );
        if let Some(prepared_head) = prepared_head {
            assert_eq!(prepared_head, artifact["id"].as_str().unwrap());
            assert_eq!(
                git(&cwd, &["rev-parse", "HEAD"]),
                prepared_head,
                "fresh checkout HEAD changed"
            );
        }
    }
    let parent = &state["tasks"]["project"];
    assert_eq!(parent["candidate"], artifact["id"]);
    assert_eq!(parent["delivery_target"], artifact["id"]);
    assert_eq!(task["candidate"], artifact["id"]);
    assert_eq!(parent["verdicts"][activity_name], *verdict);
    assert_eq!(parent["continuation"]["id"], activity_name);
    assert_eq!(parent["continuation"]["phase"], "planned");
    assert_eq!(parent["continuation"]["verdict"], verdict["id"]);
    assert_eq!(parent["continuation"]["binding"], event["binding"]);
    if let Some(dependency) = unavailable_dependency {
        assert_eq!(actual_report["conclusion"], "unable");
        assert!(parent["approved"].is_null());
        assert_eq!(task["phase"], "blocked");
        assert_eq!(parent["continuation"]["action"]["kind"], "restore");
        assert_eq!(parent["continuation"]["phase"], "planned");
        assert_eq!(parent["continuation"]["execution_problem"], J::Null);
        assert_eq!(
            state["tasks"]["project"]["requirements"]["1"], requirement_before,
            "requirement/source authorization changed"
        );
        assert_eq!(parent["candidate"], artifact["id"]);
        let task_names: std::collections::BTreeSet<_> = state["tasks"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            task_names,
            ["project", "review", "accept-native"].into_iter().collect()
        );
        assert!(
            !cwd.join(dependency).exists(),
            "operator dependency appeared"
        );
    } else if expect_pass {
        assert_eq!(actual_report["conclusion"], "passed");
        assert_eq!(parent["approved"]["candidate"], artifact["id"]);
        assert_eq!(parent["approved"]["activity"], activity_name);
        assert_eq!(parent["approved"]["round"], task["lineage"]["round"]);
        assert_eq!(parent["approved"]["verdict"], verdict["id"]);
        assert_eq!(task["phase"], "completed");
        assert_eq!(parent["continuation"]["action"]["kind"], "delivery");
    } else if expect_ambiguous {
        assert_eq!(actual_report["conclusion"], "ambiguous");
        let (head_before, file_before) = ambiguity_snapshot.as_ref().unwrap();
        assert_eq!(head_before.as_str(), artifact["id"].as_str().unwrap());
        assert_eq!(
            git(&cwd, &["rev-parse", "HEAD"]).as_str(),
            head_before.as_str()
        );
        assert_eq!(
            fs::read(cwd.join("file")).unwrap().as_slice(),
            file_before.as_slice()
        );
        let application = util::read_json(expected_application.unwrap()).unwrap();
        assert_eq!(application["task"], activity_name);
        assert_eq!(application["event"], event["id"]);
        assert_eq!(application["verdict"], verdict["id"]);
        assert_eq!(
            application["state_version"].as_u64().unwrap() + 1,
            task["state_version"].as_u64().unwrap()
        );
        assert_eq!(application["next_action"]["kind"], "clarify");
        assert!(application.get("output").is_none());
        assert!(application.get("acceptance").is_none());
        assert_eq!(actual_report["gaps"], json!([]));
        assert_eq!(actual_report["resolutions"], json!([]));
        assert_eq!(actual_report["reuses"], json!([]));
        for key in ["question", "unresolved_after_review"] {
            let value = actual_report[key]
                .as_str()
                .unwrap_or_else(|| panic!("ambiguous report lacks {key}"));
            assert!(!value.trim().is_empty() && value.len() <= 1000);
        }
        let checked: std::collections::BTreeSet<String> = actual_report["agreements_checked"]
            .as_array()
            .expect("agreements_checked array")
            .iter()
            .map(|value| value.as_str().unwrap().to_string())
            .collect();
        for source in [
            AMBIGUOUS_ORIGINAL,
            AMBIGUOUS_CHANGE,
            AMBIGUOUS_SPECIFICATION,
            AMBIGUOUS_CONFIRMATION,
            AMBIGUOUS_AUTHORIZATION_SOURCE,
        ] {
            assert!(
                checked.contains(source),
                "agreements_checked omitted {source}"
            );
        }
        let checks = actual_report["checks"]
            .as_array()
            .expect("ambiguous report checks");
        for source in [
            AMBIGUOUS_ORIGINAL,
            AMBIGUOUS_CHANGE,
            AMBIGUOUS_SPECIFICATION,
        ] {
            assert!(
                checks.iter().any(|check| {
                    check["criterion"] == "goal"
                        && check["source"] == json!(source)
                        && check["status"] == "blocked"
                }),
                "ambiguous report lacks a blocked check for {source}"
            );
        }
        assert!(parent["approved"].is_null());
        assert_eq!(task["phase"], "blocked");
        assert_eq!(task["pending"], json!([]));
        assert_eq!(parent["continuation"]["action"]["kind"], "clarify");
        assert_eq!(parent["continuation"]["phase"], "planned");
        assert_eq!(parent["continuation"]["verdict"], verdict["id"]);
        assert_eq!(parent["continuation"]["binding"], event["binding"]);
        assert_eq!(parent["requirements"]["1"], requirement_before);
        assert_eq!(parent["requirement_revision"], revision_before);
        assert_eq!(parent["candidate"], artifact["id"]);
        assert_eq!(parent["delivery_target"], artifact["id"]);
        let task_names: std::collections::BTreeSet<_> = state["tasks"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            task_names,
            ["accept-native", "project"].into_iter().collect(),
            "ambiguity phase created or retained another child"
        );
        let reviewer_commands = tool_commands(
            &reviewer_tool_rows,
            reviewer["session"].as_str().expect("reviewer session"),
        );
        let inspection = reviewer_commands
            .iter()
            .find(|command| command.command.trim() == "git show HEAD:file")
            .expect("ambiguous reviewer artifact inspection");
        assert!(
            terminal_tool_state(inspection),
            "artifact inspection not terminal"
        );
        assert_eq!(inspection.exit, Some(0));
        assert_eq!(inspection.output.trim(), "version A");
        assert!(
            inspection.start > 0
                && inspection.start != i64::MAX
                && inspection.end >= inspection.start
                && inspection.end != i64::MAX
        );
        assert_ambiguous_application_commands(
            q,
            &state,
            coordinator,
            event["id"].as_str().expect("acceptance event id"),
            expected_application.expect("ambiguity application path"),
            &coordinator_commands,
        );
    } else {
        assert_eq!(actual_report["conclusion"], "failed");
        assert!(parent["approved"].is_null());
        assert_eq!(parent["continuation"]["action"]["kind"], "repair");
    }
    state
}

fn native_repair_stage(
    env: &mut TaskEnvironment,
    q: &Path,
    guard: &Path,
    coordinator: &J,
    old_artifact: &J,
    repair_root: &Path,
    failed_report: &J,
) -> (J, J) {
    let gaps = failed_report["gaps"].as_array().unwrap();
    assert!(!gaps.is_empty(), "failed acceptance has no gaps");
    let resolution = gaps
        .iter()
        .map(|gap| {
            format!(
                "repair accept-first gap {}: {}",
                gap["id"].as_str().unwrap(),
                gap["evidence"].as_str().unwrap_or("grounded failed review")
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    let activity = acceptance_live::activity("repair-native", "development", &old_artifact["id"]);
    let spec = json!({
        "kind":"continue",
        "intent":"accept-first",
        "authorization":"develop",
        "action":"development",
        "resolution":resolution,
        "activity":activity
    });
    acceptance_live::change(q, spec);
    let cwd = acceptance_live::clone_snapshot(env, old_artifact, "repair-checkout");
    let (repair, collected) = acceptance_live::native(
        env,
        q,
        &cwd,
        "repair-native",
        "Repair the recorded failed acceptance gap against the original requirement. Change file to exactly version B and commit locally. Do not write an acceptance verdict or commit acceptance metadata. Stay within this checkout; do not inspect sibling directories, coordinator instructions, or operator repositories.",
    );
    assert_ne!(repair["session"], coordinator["session"]);
    util::atomic_write_json(
        &q.parent().unwrap().join("repair-native-release.json"),
        &json!({
            "session":repair["session"],
            "runtime":repair_root,
            "status":"server stopped and reaped after terminal collection"
        }),
    )
    .unwrap();
    let state = wait_for_stage(q, guard, coordinator, "repair-native");
    let receipt = util::read_json(&q.parent().unwrap().join("repair-native-release.json")).unwrap();
    assert_eq!(receipt["session"], repair["session"]);
    assert_eq!(receipt["session"], collected["session"]);
    assert_eq!(
        state["tasks"]["repair-native"]["last_release"]["evidence"],
        receipt["status"]
    );
    assert_stage_shape(
        &state,
        &["review", "accept-first", "repair-native"],
        coordinator,
    );
    let task = &state["tasks"]["repair-native"];
    assert_eq!(task["pending"], json!([]));
    assert_eq!(task["phase"], "completed");
    assert_eq!(task["lineage"]["input"], old_artifact["id"]);
    let event = state["events"]
        .as_object()
        .unwrap()
        .values()
        .find(|event| event["wait"] == "repair-native")
        .unwrap();
    assert_eq!(event["binding"], collected["context"]["binding"]);
    assert_eq!(
        event["evidence"],
        json!(q.join("results/repair-native.json"))
    );
    let signal = util::read_json(Path::new(event["evidence"].as_str().unwrap())).unwrap();
    assert_eq!(signal["wait"], "repair-native");
    assert_eq!(signal["outcome"], "complete");
    assert_eq!(
        signal["evidence"],
        json!(repair_root.join("submissions").join(format!(
            "{}.json",
            collected["message_id"].as_str().unwrap()
        )))
    );
    let report = util::read_json(Path::new(signal["evidence"].as_str().unwrap())).unwrap();
    assert_eq!(report["session"], repair["session"]);
    let output = collected["output"].clone();
    assert_ne!(output["id"], old_artifact["id"]);
    assert_eq!(report["output"], output);
    assert_eq!(
        git(
            Path::new(output["location"].as_str().unwrap()),
            &["show", &format!("{}:file", output["id"].as_str().unwrap())]
        ),
        "version B"
    );
    assert_eq!(
        state["tasks"]["project"]["artifacts"][output["id"].as_str().unwrap()]["object"],
        output
    );
    assert_eq!(
        state["tasks"]["project"]["activities"]["repair-native"]["output"],
        output["id"]
    );
    assert!(state["tasks"]["project"]["approved"].is_null());
    assert_eq!(state["tasks"]["project"]["candidate"], old_artifact["id"]);
    (output, repair["session"].clone())
}

// Driver registers work and collects native evidence; only the coordinator ACKs/applies.
fn native_acceptance_stage(
    env: &mut TaskEnvironment,
    q: &Path,
    guard: &Path,
    coordinator: &J,
    development: &J,
    reviewer_root: &Path,
) {
    let artifact = &development["output"];
    acceptance_live::change(
        q,
        json!({"kind":"candidate","candidate":artifact["id"],"reason":"real independent acceptance"}),
    );
    let mut activity = acceptance_live::activity("accept-native", "acceptance", &artifact["id"]);
    activity["kind"] = json!("activity");
    acceptance_live::change(q, activity);
    let cwd = acceptance_live::clone_snapshot(env, artifact, "reviewer-checkout");
    let (reviewer, collected) = acceptance_live::native(env, q, &cwd, "accept-native", "Independently inspect the exact candidate against the original requirement. Do not edit tracked files or commit. Write .herdr-verdict.json following acceptance_protocol with actual checks and grounded evidence. Derive the conclusion yourself. Do not inspect sibling directories, coordinator instructions, or operator repositories; all requirement and acceptance protocol inputs are supplied in your packet.");
    assert_ne!(reviewer["session"], development["session"]);
    assert_ne!(reviewer["session"], coordinator["session"]);
    let actual_report = util::read_json(&cwd.join(".herdr-verdict.json")).unwrap();
    let deadline = Instant::now() + Duration::from_secs(180);
    let state = loop {
        let state = queue::load_queue(q).unwrap();
        let events: Vec<_> = state["events"]
            .as_object()
            .unwrap()
            .iter()
            .filter(|(_, e)| e["wait"] == "accept-native")
            .collect();
        assert!(events.len() <= 1, "duplicate acceptance event");
        if let Some((id, event)) = events.first() {
            let receipt = state["deliveries"]
                .as_object()
                .unwrap()
                .iter()
                .find(|(_, d)| {
                    d["events"] == json!([id])
                        && d["phase"] == "acknowledged"
                        && d["session"] == coordinator["session"]
                });
            if let Some((delivery, _)) = receipt {
                let readiness = util::read_json(&q.join("readiness.json")).unwrap_or(J::Null);
                let submission = util::read_json(&guard.join("submission.json")).unwrap_or(J::Null);
                assert_ne!(
                    submission["native_status"], "blocked",
                    "native approval blocked acceptance"
                );
                if event["applied"] == true
                    && readiness["active_wakeup"] == true
                    && readiness["last_confirmed_receipt"]["delivery"] == *delivery
                    && readiness["last_confirmed_receipt"]["session"] == coordinator["session"]
                    && submission["phase"] == "complete"
                {
                    break state;
                }
            }
        }
        assert!(
            Instant::now() < deadline,
            "native acceptance application deadline"
        );
        std::thread::sleep(Duration::from_millis(200));
    };
    assert_eq!(state["events"].as_object().unwrap().len(), 2);
    assert_eq!(state["deliveries"].as_object().unwrap().len(), 2);
    for wait in ["review", "accept-native"] {
        let matches: Vec<_> = state["events"]
            .as_object()
            .unwrap()
            .iter()
            .filter(|(_, e)| e["wait"] == wait)
            .collect();
        assert_eq!(matches.len(), 1);
        let (id, event) = matches[0];
        assert_eq!(event["acknowledged"], true);
        assert_eq!(event["applied"], true);
        let deliveries: Vec<_> = state["deliveries"]
            .as_object()
            .unwrap()
            .values()
            .filter(|d| d["events"] == json!([id]))
            .collect();
        assert_eq!(deliveries.len(), 1);
        assert_eq!(deliveries[0]["session"], coordinator["session"]);
        assert_eq!(deliveries[0]["phase"], "acknowledged");
    }
    let task = &state["tasks"]["accept-native"];
    assert_eq!(task["pending"], json!([]));
    let verdict = &task["verdict"];
    let event = state["events"]
        .as_object()
        .unwrap()
        .values()
        .find(|e| e["wait"] == "accept-native")
        .unwrap();
    assert_eq!(event["binding"], collected["context"]["binding"]);
    assert_eq!(verdict["envelope"]["binding"], event["binding"]);
    assert_eq!(
        verdict["envelope"]["reviewer"]["session"],
        reviewer["session"]
    );
    assert_eq!(verdict["envelope"]["report"], actual_report);
    assert_eq!(verdict["envelope"]["reviewer"]["adapter"], "opencode");
    assert_eq!(
        util::read_json(&cwd.join(".herdr-verdict.json")).unwrap(),
        actual_report
    );
    assert_eq!(event["outcome"], "complete");
    assert_eq!(event["acknowledged"], true);
    assert_eq!(
        event["evidence"],
        json!(q.join("results/accept-native.json"))
    );
    let signal = util::read_json(Path::new(event["evidence"].as_str().unwrap())).unwrap();
    assert_eq!(signal["wait"], "accept-native");
    assert_eq!(signal["outcome"], "complete");
    assert_eq!(
        signal["evidence"],
        json!(reviewer_root.join("submissions").join(format!(
            "{}.json",
            collected["message_id"].as_str().unwrap()
        )))
    );
    let report = util::read_json(Path::new(signal["evidence"].as_str().unwrap())).unwrap();
    assert_eq!(report["session"], reviewer["session"]);
    let parent = &state["tasks"]["project"];
    assert_eq!(parent["candidate"], artifact["id"]);
    assert_eq!(parent["delivery_target"], artifact["id"]);
    assert_eq!(task["candidate"], artifact["id"]);
    assert_eq!(parent["verdicts"]["accept-native"], *verdict);
    assert_eq!(parent["continuation"]["id"], "accept-native");
    assert_eq!(parent["continuation"]["phase"], "planned");
    assert_eq!(parent["continuation"]["verdict"], verdict["id"]);
    assert_eq!(parent["continuation"]["binding"], event["binding"]);
    if actual_report["conclusion"] == "passed" {
        assert_eq!(parent["approved"]["candidate"], artifact["id"]);
        assert_eq!(parent["approved"]["activity"], "accept-native");
        assert_eq!(parent["approved"]["round"], task["lineage"]["round"]);
        assert_eq!(parent["approved"]["verdict"], verdict["id"]);
        assert_eq!(task["phase"], "completed");
        assert_eq!(parent["continuation"]["action"]["kind"], "delivery");
    } else {
        assert!(parent["approved"].is_null());
    }
    assert_eq!(
        actual_report["conclusion"], "passed",
        "independent reviewer did not accept the candidate; no pass manufactured"
    );
}
