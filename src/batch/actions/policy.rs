use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::ci;
use crate::error::{FlowError, Result};
use crate::model::J;
use crate::util::pyjson::sha256_json_sorted;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Policy {
    version: u32,
    kind: String,
    workflows: Vec<Workflow>,
    landing_workflows: Vec<Workflow>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Workflow {
    file: String,
    event: String,
    decision: String,
    jobs: Vec<String>,
}

pub(super) fn require(ok: bool, message: &str) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(FlowError::progress(message))
    }
}

pub(super) fn validate(value: &J) -> Result<J> {
    let policy: Policy = serde_json::from_value(value.clone())
        .map_err(|_| FlowError::progress("invalid explicit Actions lane policy"))?;
    require(
        policy.version == 1 && policy.kind == "github-actions",
        "unsupported Actions lane policy",
    )?;
    inventory(&policy.workflows, "pull_request")?;
    inventory(&policy.landing_workflows, "push")?;
    serde_json::to_value(policy).map_err(|_| FlowError::progress("invalid Actions policy"))
}

fn inventory(rows: &[Workflow], event: &str) -> Result<()> {
    require(
        (1..=16).contains(&rows.len()),
        "Actions inventory requires 1..16 workflows",
    )?;
    for row in rows {
        require(
            row.event == event && row.decision == "required_jobs",
            "exact Actions event and required_jobs decision required",
        )?;
        jobs(&row.jobs)?;
    }
    let workflows = serde_json::to_value(rows)
        .map_err(|_| FlowError::progress("invalid workflow inventory"))?;
    ci::validate(
        &json!({"version":1,"repo":"owner/repo","task":"policy-validation", "epoch":1,
        "sha":"a".repeat(40),"workflows":workflows}),
    )?;
    Ok(())
}

fn jobs(names: &[String]) -> Result<()> {
    require(
        (1..=64).contains(&names.len()),
        "Actions workflow requires 1..64 exact jobs",
    )?;
    let unique: BTreeSet<_> = names.iter().collect();
    require(unique.len() == names.len(), "duplicate required job")?;
    require(
        names
            .iter()
            .all(|name| !name.trim().is_empty() && name.len() <= 250),
        "invalid required job name",
    )
}

pub(crate) fn enabled(lane: &J) -> bool {
    lane.get("ci").is_some()
}

pub(crate) fn check(lane: &J) -> Result<()> {
    if !enabled(lane) {
        return Ok(());
    }
    require(
        lane.get("gate").is_none_or(J::is_null),
        "mixed Gate and Actions lane",
    )?;
    let policy = validate(&lane["ci"])?;
    require(policy == lane["ci"], "noncanonical Actions policy")?;
    require(
        lane["ci_policy_digest"] == json!(sha256_json_sorted(&policy)),
        "Actions policy digest changed",
    )
}

pub(crate) fn initialize(spec: &J) -> Result<Option<J>> {
    if let Some(ci) = spec.get("ci") {
        require(
            spec.get("gate").is_none(),
            "choose exactly one of gate or ci",
        )?;
        return validate(ci).map(Some);
    }
    Ok(None)
}
