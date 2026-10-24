use super::{enabled, observation, policy::require, saved};
use crate::ci::GitHubApi;
use crate::error::Result;
use crate::model::{self, J};
use crate::util::pyjson::sha256_json_sorted;
use serde_json::json;

pub(crate) fn repair_token(batch: &J) -> String {
    sha256_json_sorted(&json!({"batch":batch["id"],"revision":batch["revision"],"ci":batch["ci"]}))
}

pub(crate) fn check_saved_failure(t: &J, lane: &J, batch: &J) -> Result<()> {
    let previous = saved(lane, batch)?;
    super::owner(t, &previous)?;
    require(
        previous["state"] == "failure",
        "only failed required CI authorizes batch repair",
    )?;
    require(
        batch["failure_token"] == repair_token(batch),
        "Actions failure token changed",
    )
}

pub(crate) fn check_intent(t: &J, lane: &J, batch: &J, now: f64) -> Result<()> {
    super::check(lane)?;
    if !enabled(lane) {
        return require(
            batch["phase"] != "ci_failed",
            "Actions failure requires an explicit Actions lane",
        );
    }
    if batch["phase"] != "ci_failed" {
        return Ok(());
    }
    check_saved_failure(t, lane, batch)?;
    let at = model::number(&batch["ci"], "observed_at").unwrap_or(f64::NEG_INFINITY);
    require(
        (0.0..=120.0).contains(&(now - at)),
        "Actions failure observation is stale",
    )
}

pub(crate) fn check_failure(t: &J, lane: &J, batch: &J, api: &dyn GitHubApi) -> Result<()> {
    check_saved_failure(t, lane, batch)?;
    let current = observation::head(lane, batch, &batch["ci"]["pr"], false, api)?;
    require(
        current["state"] == "failure" && current["workflows"] == batch["ci"]["workflows"],
        "CI failure/run/attempt changed before repair",
    )
}

pub(crate) fn check_repair_source(
    t: &J,
    lane: &J,
    batch: &J,
    source: &J,
    api: &dyn GitHubApi,
) -> Result<()> {
    require(
        source["id"] == batch["id"] && source["revision"] == batch["revision"],
        "Actions repair source batch/revision changed",
    )?;
    check_failure(t, lane, source, api)
}
