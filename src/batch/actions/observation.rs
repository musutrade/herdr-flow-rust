use super::{config, policy::require};
use crate::ci::{self, GitHubApi};
use crate::error::Result;
use crate::model::{self, J};
use serde_json::json;

pub(super) fn head(lane: &J, batch: &J, pr: &J, merged: bool, api: &dyn GitHubApi) -> Result<J> {
    pull(lane, batch, pr, merged, api)?;
    let config = config(lane, &batch["snapshot"]["head"], "workflows", pr);
    ci::validate(&config)?;
    let result = verified(&config, api)?;
    pull(lane, batch, pr, merged, api)?;
    Ok(result)
}

/// Only a saved strict premerge success can authorize the postmerge-empty path.
pub(super) fn landing_head(lane: &J, batch: &J, previous: &J, api: &dyn GitHubApi) -> Result<J> {
    if previous["pr_membership"] != "verified" {
        return head(lane, batch, &previous["pr"], true, api);
    }
    require(
        previous["state"] == "success",
        "missing verified premerge success",
    )?;
    let pr = &previous["pr"];
    pull(lane, batch, pr, true, api)?;
    let config = config(lane, &batch["snapshot"]["head"], "workflows", pr);
    ci::validate(&config)?;
    let rows = model::array_field(previous, "workflows");
    require(
        rows.map(Vec::len) == model::array_field(&config, "workflows").map(Vec::len),
        "incomplete saved premerge workflows",
    )?;
    let result = verified_rows(&config, rows.map(Vec::as_slice), api)?;
    pull(lane, batch, pr, true, api)?;
    Ok(result)
}

pub(super) fn pull(lane: &J, batch: &J, pr: &J, merged: bool, api: &dyn GitHubApi) -> Result<J> {
    let number = model::int(pr, "number").unwrap_or(0);
    require(number > 0, "registered PR number required")?;
    let remote = api.get(&format!("pulls/{number}"))?;
    require(
        remote["number"] == pr["number"],
        "remote PR identity changed",
    )?;
    require(
        remote["head"]["sha"] == batch["snapshot"]["head"]
            && remote["head"]["ref"] == pr["head_ref"],
        "remote PR head changed",
    )?;
    require(
        remote["head"]["repo"]["full_name"] == lane["repo"]
            && remote["base"]["repo"]["full_name"] == lane["repo"],
        "remote PR repository changed",
    )?;
    require(
        remote["base"]["ref"] == lane["base_ref"],
        "remote PR base ref changed",
    )?;
    state(lane, batch, &remote, merged, api)?;
    Ok(remote)
}

fn state(lane: &J, batch: &J, remote: &J, merged: bool, api: &dyn GitHubApi) -> Result<()> {
    if merged {
        return require(
            remote["merged"] == true && remote["state"] == "closed",
            "PR is not a confirmed merge",
        );
    }
    super::no_auto_merge(remote)?;
    require(
        remote["merged"] == false && remote["state"] == "open",
        "PR is not open for CI observation/repair",
    )?;
    require(
        remote["base"]["sha"] == batch["base"],
        "PR base changed; rebase and revalidate",
    )?;
    require(
        super::super::validation::latest(lane, api)? == batch["base"].as_str().unwrap_or_default(),
        "target changed during CI observation",
    )
}

/// Two bounded observations detect superseding runs/attempts during collection.
/// This is a read-only revalidation, not a retry of any external mutation.
pub(super) fn verified(config: &J, api: &dyn GitHubApi) -> Result<J> {
    verified_rows(config, None, api)
}

fn verified_rows(config: &J, saved: Option<&[J]>, api: &dyn GitHubApi) -> Result<J> {
    let first = collect(config, saved, api)?;
    let second = collect(config, saved, api)?;
    require(
        first == second,
        "CI changed during observation; inspect current runs",
    )?;
    Ok(second)
}

fn collect(config: &J, saved: Option<&[J]>, api: &dyn GitHubApi) -> Result<J> {
    let mut result = ci::observe(config, api)?;
    let specs = model::array_field(config, "workflows")
        .cloned()
        .unwrap_or_default();
    let rows = model::array_field(&result, "workflows")
        .cloned()
        .unwrap_or_default();
    require(
        specs.len() == rows.len(),
        "incomplete Actions workflow observation",
    )?;
    let mut compact = Vec::new();
    for (index, (spec, row)) in specs.iter().zip(rows.iter()).enumerate() {
        compact.push(row_checked(
            config,
            spec,
            row,
            saved.and_then(|rows| rows.get(index)),
            api,
        )?);
    }
    result["workflows"] = json!(compact);
    // A failure cannot authorize repair while any other required workflow is
    // missing or unfinished. Preserve ambiguous evidence for operator review.
    if result["state"] != "needs_attention" && !compact.iter().all(terminal_decision) {
        result["state"] = json!("pending");
    }
    Ok(result)
}

fn terminal_decision(row: &J) -> bool {
    matches!(row["state"].as_str(), Some("success" | "failure"))
        && row["run_status"] == "completed"
        && row["decision_terminal"] == true
}

fn row_checked(config: &J, spec: &J, row: &J, saved: Option<&J>, api: &dyn GitHubApi) -> Result<J> {
    require(
        row["file"] == spec["file"] && row["event"] == spec["event"],
        "CI workflow/event mismatch",
    )?;
    if row["state"] == "missing" {
        return Ok(row.clone());
    }
    let result = required_row(spec, row)?;
    if let Some(previous) = saved {
        same_run(spec, &result, previous)?;
    }
    current_run(config, spec, row, saved.is_some(), api)?;
    Ok(result)
}

fn same_run(spec: &J, live: &J, previous: &J) -> Result<()> {
    require(
        live["state"] == "success" && terminal_decision(live),
        "landing requires terminal head success",
    )?;
    require(
        previous["state"] == "success" && terminal_decision(previous),
        "saved premerge workflow is not terminal success",
    )?;
    let previous = required_row(spec, previous)?;
    require(
        run_identity(live) == run_identity(&previous),
        "premerge run/attempt/workflow/jobs changed before landing",
    )
}

fn run_identity(row: &J) -> J {
    let jobs: Vec<J> = model::array_field(row, "jobs").into_iter().flatten().map(|job| {
        json!({"id":job["id"],"name":job["name"],"status":job["status"],"conclusion":job["conclusion"]})
    }).collect();
    json!({"file":row["file"],"event":row["event"],"run_id":row["run_id"],"attempt":row["attempt"],"jobs":jobs})
}

fn current_run(
    config: &J,
    spec: &J,
    row: &J,
    allow_empty: bool,
    api: &dyn GitHubApi,
) -> Result<()> {
    let id = model::int(row, "run_id").unwrap_or(0);
    let attempt = model::int(row, "attempt").unwrap_or(0);
    require(id > 0 && attempt > 0, "invalid Actions run identity")?;
    let current = api.get(&format!("actions/runs/{id}"))?;
    unchanged_run(row, &current)?;
    require(
        current["head_sha"] == config["sha"] && current["event"] == spec["event"],
        "Actions run head/event changed",
    )?;
    let expected_path = format!(
        ".github/workflows/{}",
        spec["file"].as_str().unwrap_or_default()
    );
    require(
        current["path"] == expected_path,
        "Actions workflow path changed",
    )?;
    run_target(config, spec, &current, allow_empty)
}

fn unchanged_run(row: &J, current: &J) -> Result<()> {
    require(
        current["id"] == row["run_id"] && current["run_attempt"] == row["attempt"],
        "Actions run/attempt changed",
    )?;
    require(
        current["status"] == row["run_status"],
        "Actions run status changed during observation",
    )
}

fn required_row(spec: &J, row: &J) -> Result<J> {
    require(
        row["decision"] == "required_jobs",
        "CI must use required_jobs policy",
    )?;
    let names = model::array_field(spec, "jobs")
        .cloned()
        .unwrap_or_default();
    let jobs = model::array_field(row, "jobs").cloned().unwrap_or_default();
    let mut selected = Vec::new();
    for name in &names {
        let matches: Vec<_> = jobs.iter().filter(|job| job["name"] == *name).collect();
        require(matches.len() <= 1, "duplicate required job identity")?;
        if let Some(job) = matches.first() {
            require(
                model::int(job, "id").is_some_and(|id| id > 0),
                "missing required job id",
            )?;
            selected.push((*job).clone());
        }
    }
    let mut result = row.clone();
    result["jobs"] = json!(selected);
    if row["state"] == "success" {
        successful(row, &names, &selected)?;
    }
    Ok(result)
}

fn successful(row: &J, names: &[J], jobs: &[J]) -> Result<()> {
    require(
        row["decision_reason"] == "required_jobs_succeeded" && row["decision_terminal"] == true,
        "incomplete required-job decision",
    )?;
    require(
        row["missing_jobs"] == json!([]) && jobs.len() == names.len(),
        "missing required jobs",
    )?;
    require(
        jobs.iter()
            .all(|job| job["status"] == "completed" && job["conclusion"] == "success"),
        "required job not successful",
    )
}

fn run_target(config: &J, spec: &J, run: &J, allow_empty: bool) -> Result<()> {
    require(
        run["head_repository"]["full_name"] == config["repo"],
        "Actions head repository changed",
    )?;
    if spec["event"] == "push" {
        return require(
            run["head_branch"] == config["lane_base_ref"],
            "merge-main run branch changed",
        );
    }
    require(
        run["head_branch"] == config["lane_head_ref"],
        "PR run branch changed",
    )?;
    require(
        membership(run, &config["lane_pr"], allow_empty),
        "Actions run is not bound to the registered PR",
    )
}

fn membership(run: &J, number: &J, allow_empty: bool) -> bool {
    let Some(prs) = model::array_field(run, "pull_requests") else {
        return false;
    };
    if prs.is_empty() {
        return allow_empty;
    }
    prs.iter()
        .all(|pr| model::int(pr, "number").is_some_and(|n| n > 0))
        && prs.iter().any(|pr| pr["number"] == *number)
}
