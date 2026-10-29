//! Atomic business outcome and pending cleanup obligation; no finalizer here.
use super::{ensure, progress_error, text};
use crate::{lifecycle::Owner, model::J, task_env::TaskEnvironment, Result};
use serde_json::json;

pub fn ensure_open(s: &J, name: &str) -> Result<()> {
    if s["lifecycle"].is_null() {
        return Ok(());
    }
    let t = &s["tasks"][name];
    let owner = t["lineage"]["requirement"].as_str().unwrap_or(name);
    let t = &s["tasks"][owner];
    ensure(
        t["final_outcome"].is_null() && t["cleanup_obligation"].is_null(),
        "task instance is closing",
    )
}

pub fn record(s: &mut J, spec: &J, owner: &Owner, env: &TaskEnvironment, now: f64) -> Result<()> {
    let outcome = text(spec.get("outcome"), "terminal outcome", 16)?;
    ensure(
        matches!(outcome.as_str(), "success" | "failure"),
        "terminal outcome must be success or failure",
    )?;
    let reason = text(spec.get("reason"), "terminal reason", 1000)?;
    let t = &s["tasks"][&owner.task_id];
    ensure(
        t["lineage"]["requirement"].is_null(),
        "only the lifecycle owner may become terminal",
    )?;
    if !t["final_outcome"].is_null() {
        return repeated(t, owner, &outcome, &reason);
    }
    validate_new(t, spec, env, &outcome)?;
    commit(s, owner, &outcome, &reason, now)
}

fn repeated(t: &J, owner: &Owner, outcome: &str, reason: &str) -> Result<()> {
    ensure(
        t["final_outcome"] == outcome
            && t["final_reason"] == reason
            && t["cleanup_obligation"]["owner"] == serde_json::to_value(owner)?,
        "terminal outcome is immutable; inspect existing cleanup obligation",
    )
}

fn validate_new(t: &J, spec: &J, env: &TaskEnvironment, outcome: &str) -> Result<()> {
    ensure(
        human_terminal_allowed(&t["human_writer"], outcome),
        "return and verify the human writer, or explicitly human-abandon before failure finalization; retain its checkout",
    )?;
    ensure(
        t["state_version"] == spec["state_version"] && spec["state_version"].as_u64().is_some(),
        "task state version changed; inspect before terminal submission",
    )?;
    ensure(
        t["cleanup_obligation"].is_null(),
        "cleanup obligation exists without terminal outcome",
    )?;
    if outcome == "success" {
        return confirmed_delivery(t, env);
    }
    Ok(())
}

fn human_terminal_allowed(record: &J, outcome: &str) -> bool {
    record.is_null()
        || matches!(record["phase"].as_str(), Some("draining" | "released"))
        || (record["phase"] == "abandoned"
            && record["abandon"]["acknowledge_cleanup"] == true
            && outcome == "failure")
}

fn confirmed_delivery(parent: &J, env: &TaskEnvironment) -> Result<()> {
    ensure(
        parent["stage"].is_null() && env.delivery["stage"].is_null(),
        "stage delivery does not satisfy final requirement acceptance",
    )?;
    ensure(
        env.delivery["phase"] == "confirmed"
            && env.delivery["instance"] == env.instance_id
            && env.delivery["task"] == env.task_id,
        "final success requires confirmed delivery for this task instance",
    )
}

fn commit(s: &mut J, owner: &Owner, outcome: &str, reason: &str, now: f64) -> Result<()> {
    let t = &mut s["tasks"][&owner.task_id];
    let version = t["state_version"]
        .as_u64()
        .and_then(|n| n.checked_add(1))
        .ok_or_else(|| progress_error("invalid task state version"))?;
    t["final_outcome"] = json!(outcome);
    t["final_reason"] = json!(reason);
    t["finalized_at"] = json!(now);
    t["phase"] = json!("finalizing");
    t["state_version"] = json!(version);
    t["cleanup_obligation"] = json!({"owner":owner,"status":"pending","recorded_at":now});
    Ok(())
}
