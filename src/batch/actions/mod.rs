//! Explicit Actions policy for the existing bounded lane. Never manufactures a Gate receipt.
mod observation;
mod policy;
mod repair;

use crate::ci::{self, GitHubApi};
use crate::error::Result;
use crate::model::{self, J};
use crate::util::pyjson::sha256_json_sorted;
use policy::require;
pub(crate) use policy::{check, enabled, initialize};
pub(crate) use repair::{
    check_failure, check_intent, check_repair_source, check_saved_failure, repair_token,
};
use serde_json::json;

pub(crate) fn publication(lane: &J, batch: &J) -> Result<()> {
    check(lane)?;
    require(
        batch["gate"].is_null() && batch["ci"].is_null(),
        "Actions publication requires no prior Gate/CI result",
    )?;
    require(
        batch["phase"] == "snapshot_frozen",
        "Actions publication requires a released fresh snapshot",
    )
}

pub(crate) fn observe(t: &J, lane: &J, batch: &J, now: f64, api: &dyn GitHubApi) -> Result<J> {
    check(lane)?;
    require(
        enabled(lane),
        "batch-ci-observe requires an explicit Actions lane",
    )?;
    require(
        batch["gate"].is_null(),
        "Actions batch cannot contain a Gate result",
    )?;
    require(
        matches!(
            batch["phase"].as_str(),
            Some("snapshot_frozen" | "ci_pending" | "ci_failed" | "ci_passed")
        ),
        "batch is not awaiting PR CI",
    )?;
    require(
        observation_budget(batch),
        "Actions observation limit reached; inspect the batch",
    )?;
    snapshot_identity(batch)?;
    let pr = registered(t, lane, batch)?;
    let result = observation::head(lane, batch, &pr, false, api)?;
    Ok(
        json!({"kind":"github-actions","policy_digest":lane["ci_policy_digest"],
        "binding":binding(lane,batch),"pr":pr,"observed_at":now,
        "state":result["state"],"workflows":result["workflows"]}),
    )
}

fn registered(t: &J, lane: &J, batch: &J) -> Result<J> {
    let pr = &t["pr"];
    require(
        model::int(pr, "number").is_some_and(|n| n > 0),
        "run pr-reconcile before batch-ci-observe",
    )?;
    require(
        matches!(
            pr["phase"].as_str(),
            Some("registered" | "native_merge_not_enabled" | "repair_needed" | "checks_satisfied")
        ),
        "run pr-reconcile for the existing open PR first",
    )?;
    require(
        pr["head"] == batch["snapshot"]["head"] && pr["target_base"] == batch["base"],
        "PR snapshot/base mismatch",
    )?;
    require(
        pr["repo"] == lane["repo"]
            && pr["base_ref"] == lane["base_ref"]
            && pr["host"] == lane["host"],
        "PR target mismatch",
    )?;
    Ok(json!({"number":pr["number"],"head_ref":pr["head_ref"],"task":t["task"],"epoch":t["epoch"]}))
}

pub(super) fn binding(lane: &J, batch: &J) -> J {
    json!({"repo":lane["repo"],"host":lane["host"],"base_ref":lane["base_ref"],
        "base":batch["base"],"head":batch["snapshot"]["head"],"tree":batch["snapshot"]["tree"]})
}

pub(super) fn saved(lane: &J, batch: &J) -> Result<J> {
    check(lane)?;
    require(
        enabled(lane),
        "Actions observation requires an explicit Actions lane",
    )?;
    snapshot_identity(batch)?;
    let value = batch["ci"].clone();
    require(
        value["kind"] == "github-actions" && value["policy_digest"] == lane["ci_policy_digest"],
        "missing or changed Actions observation policy",
    )?;
    require(
        value["binding"] == binding(lane, batch),
        "Actions observation binding changed",
    )?;
    require(
        model::int(&value["pr"], "number").is_some_and(|n| n > 0),
        "missing registered Actions PR",
    )?;
    Ok(value)
}

pub(crate) fn landing(t: &J, lane: &J, batch: &J, landing: &J, api: &dyn GitHubApi) -> Result<()> {
    let previous = saved(lane, batch)?;
    require(
        previous["state"] == "success",
        "merged batch lacks a successful pre-merge CI observation; retain slot",
    )?;
    owner(t, &previous)?;
    require(
        previous["pr"]["number"] == t["pr"]["number"]
            && previous["pr"]["head_ref"] == t["pr"]["head_ref"],
        "Actions PR changed before landing",
    )?;
    landing_ancestry(batch, landing, api)?;
    let head = observation::head(lane, batch, &previous["pr"], true, api)?;
    require(
        head["state"] == "success",
        "merged batch head CI is not successful; retain slot",
    )?;
    landing_main(lane, batch, landing, &previous["pr"], api)
}

fn landing_main(lane: &J, batch: &J, landing: &J, pr: &J, api: &dyn GitHubApi) -> Result<()> {
    let config = config(lane, &landing["commit"], "landing_workflows", pr);
    ci::validate(&config)?;
    let main = observation::verified(&config, api)?;
    require(
        main["state"] == "success",
        &format!(
            "merge-main CI {}; retain slot, no automatic repair",
            main["state"].as_str().unwrap_or("unknown")
        ),
    )?;
    let remote = observation::pull(lane, batch, pr, true, api)?;
    require(
        remote["merge_commit_sha"] == landing["commit"],
        "landing commit changed",
    )
}

pub(super) fn config(lane: &J, sha: &J, key: &str, pr: &J) -> J {
    json!({"version":1,"task":pr["task"],"epoch":pr["epoch"],"repo":lane["repo"],"host":lane["host"],
        "sha":sha,"workflows":lane["ci"][key],"lane_pr":pr["number"],"lane_head_ref":pr["head_ref"],"lane_base_ref":lane["base_ref"]})
}

pub(crate) fn digest(policy: &J) -> String {
    sha256_json_sorted(policy)
}

#[cfg(test)]
#[path = "../../../tests/common/batch_actions.rs"]
mod tests;

fn landing_ancestry(batch: &J, landing: &J, api: &dyn GitHubApi) -> Result<()> {
    let base = crate::pr::sha(Some(&batch["base"]))?;
    let commit = crate::pr::sha(Some(&landing["commit"]))?;
    let value = api.get(&format!("compare/{base}...{commit}"))?;
    require(
        matches!(value["status"].as_str(), Some("ahead" | "identical"))
            && value["merge_base_commit"]["sha"] == base,
        "landing does not descend from the observed base; retain slot",
    )
}

fn snapshot_identity(batch: &J) -> Result<()> {
    crate::pr::sha(Some(&batch["base"]))?;
    crate::pr::sha(Some(&batch["snapshot"]["head"]))?;
    crate::pr::sha(Some(&batch["snapshot"]["tree"]))?;
    Ok(())
}

fn observation_budget(batch: &J) -> bool {
    batch
        .get("ci_observations")
        .is_none_or(|n| n.as_u64().is_some_and(|n| n < 256))
}

pub(super) fn owner(t: &J, observation: &J) -> Result<()> {
    require(
        observation["pr"]["task"] == t["task"] && observation["pr"]["epoch"] == t["epoch"],
        "Actions task/epoch identity changed",
    )
}

pub(crate) fn publication_settings(t: &J, api: &dyn GitHubApi) -> Result<()> {
    let Some(number) = model::int(&t["pr"], "number") else {
        return Ok(());
    };
    require(number > 0, "invalid existing Actions PR number")?;
    let remote = api.get(&format!("pulls/{number}"))?;
    require(
        remote["number"] == number && remote["state"] == "open" && remote["merged"] == false,
        "existing Actions publication is not the open PR",
    )?;
    no_auto_merge(&remote)
}

pub(super) fn no_auto_merge(pr: &J) -> Result<()> {
    require(
        !model::truthy(pr.get("auto_merge")),
        "Actions lane requires auto-merge disabled",
    )
}
