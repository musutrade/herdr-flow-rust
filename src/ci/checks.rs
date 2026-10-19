//! Read-only external Checks policy. No credentials, publication or quality
//! verdicts. The caller pins `external_id` from the trusted gate's run receipt.

use std::collections::BTreeSet;

use serde_json::{json, Value};

use crate::ci::watch::GitHubApi;
use crate::error::{FlowError, Result};
use crate::model::{self, J};
use crate::util::pyjson::quote_component;

fn watch(message: impl Into<String>) -> FlowError {
    FlowError::watch(message)
}

fn validate_check_item(item: &J) -> Result<(String, i64)> {
    let Some(map) = item.as_object() else {
        return Err(watch("each check requires exactly name/app_id/external_id"));
    };
    let keys: BTreeSet<&str> = map.keys().map(String::as_str).collect();
    if keys != BTreeSet::from(["name", "app_id", "external_id"]) {
        return Err(watch("each check requires exactly name/app_id/external_id"));
    }
    for key in ["name", "external_id"] {
        match model::string(item, key) {
            Some(value) if !value.trim().is_empty() && value.chars().count() <= 500 => {}
            _ => return Err(watch(format!("invalid external check {key}"))),
        }
    }
    let Some(app_id) = model::int(item, "app_id") else {
        return Err(watch("external check requires a positive App ID"));
    };
    if app_id <= 0 {
        return Err(watch("external check requires a positive App ID"));
    }
    Ok((model::string(item, "name").unwrap_or_default(), app_id))
}

fn validate_check_inputs(config: &J) -> Result<()> {
    let pr = model::get(config, "pr").and_then(Value::as_i64);
    let sha_ok = model::string(config, "sha")
        .map(|value| model::is_full_sha(&value))
        .unwrap_or(false);
    let repo_ok = model::string(config, "repo")
        .map(|value| {
            regex::Regex::new(r"^[\w.-]+/[\w.-]+$")
                .expect("static repo regex")
                .is_match(&value)
        })
        .unwrap_or(false);
    if pr.unwrap_or(0) < 1 || !sha_ok || !repo_ok {
        return Err(watch(
            "external checks require a positive PR, full SHA and explicit owner/repo",
        ));
    }
    let target_ok = model::string(config, "target_base")
        .map(|value| model::is_full_sha(&value))
        .unwrap_or(false);
    let base_ok = model::string(config, "base_ref")
        .map(|value| !value.is_empty())
        .unwrap_or(false);
    if !target_ok || !base_ok {
        return Err(watch(
            "external checks require PR, base_ref and full target_base",
        ));
    }
    Ok(())
}

fn validate_inventory(config: &J) -> Result<Vec<J>> {
    if !config.is_object() {
        return Err(watch("configuration must be a JSON object"));
    }
    let checks = match model::get(config, "checks") {
        None => Vec::new(),
        Some(Value::Array(items)) if items.len() <= 32 => items.clone(),
        _ => return Err(watch("checks must be an explicit inventory (max 32)")),
    };
    let mut keys = BTreeSet::new();
    for item in &checks {
        if !keys.insert(validate_check_item(item)?) {
            return Err(watch("duplicate external check name/App"));
        }
    }
    Ok(checks)
}

pub fn validate(config: &J) -> Result<Vec<J>> {
    let checks = validate_inventory(config)?;
    if !checks.is_empty() {
        validate_check_inputs(config)?;
    }
    Ok(checks)
}

pub fn observe(config: &J, api: &dyn GitHubApi) -> Result<Vec<J>> {
    validate(config).and_then(|checks| observe_with_checks(config, api, checks))
}

fn observe_with_checks(config: &J, api: &dyn GitHubApi, checks: Vec<J>) -> Result<Vec<J>> {
    if checks.is_empty() {
        return Ok(Vec::new());
    }
    observe_open_pull(config, api)
}

fn observe_open_pull(config: &J, api: &dyn GitHubApi) -> Result<Vec<J>> {
    let pr_path = pull_path(config);
    let base_ref = model::string(config, "base_ref").unwrap_or_default();
    observe_initial_snapshot(config, api, &pr_path, &base_ref)
}

fn pull_path(config: &J) -> String {
    format!("pulls/{}", model::int(config, "pr").unwrap_or_default())
}

fn branch_path(base_ref: &str) -> String {
    format!("branches/{}", quote_component(base_ref))
}

fn observe_initial_snapshot(
    config: &J,
    api: &dyn GitHubApi,
    pr_path: &str,
    base_ref: &str,
) -> Result<Vec<J>> {
    let latest_path = branch_path(base_ref);
    api.get(pr_path)
        .and_then(|pr| api.get(&latest_path).map(|latest| (pr, latest)))
        .and_then(|(pr, latest)| {
            observe_initial_check(config, api, pr_path, &latest_path, pr, latest)
        })
}

fn observe_initial_check(
    config: &J,
    api: &dyn GitHubApi,
    pr_path: &str,
    latest_path: &str,
    pr: J,
    latest: J,
) -> Result<Vec<J>> {
    if check_inputs_changed(config, &pr, &latest) {
        return Ok(check_inputs_changed_row());
    }
    observe_after_run_rows(config, api, pr_path, latest_path)
}

fn check_inputs_changed_row() -> Vec<J> {
    vec![json!({
        "state": "needs_attention",
        "reason": "external_check_inputs_changed",
        "decision_terminal": true
    })]
}

fn observe_after_run_rows(
    config: &J,
    api: &dyn GitHubApi,
    pr_path: &str,
    latest_path: &str,
) -> Result<Vec<J>> {
    run_rows(config, api)
        .and_then(|rows| observe_final_snapshot(config, api, pr_path, latest_path, rows))
}

fn observe_final_snapshot(
    config: &J,
    api: &dyn GitHubApi,
    pr_path: &str,
    latest_path: &str,
    rows: Vec<J>,
) -> Result<Vec<J>> {
    api.get(pr_path)
        .and_then(|pr| api.get(latest_path).map(|latest| (pr, latest)))
        .and_then(|(pr, latest)| observe_final_check(config, pr, latest, rows))
}

fn observe_final_check(config: &J, pr: J, latest: J, rows: Vec<J>) -> Result<Vec<J>> {
    if check_inputs_changed(config, &pr, &latest) {
        return Ok(check_inputs_changed_row());
    }
    Ok(rows)
}

fn check_inputs_changed(config: &J, pr: &J, latest: &J) -> bool {
    let repo = model::string(config, "repo").unwrap_or_default();
    model::dig_str(pr, &["head", "sha"]) != model::string(config, "sha")
        || model::dig_str(pr, &["base", "ref"]) != model::string(config, "base_ref")
        || model::dig_str(pr, &["base", "repo", "full_name"]) != Some(repo.clone())
        || model::dig_str(pr, &["head", "repo", "full_name"]) != Some(repo)
        || model::dig_str(pr, &["base", "sha"]) != model::string(config, "target_base")
        || model::dig_str(latest, &["commit", "sha"]) != model::string(config, "target_base")
        || model::string(pr, "state").as_deref() != Some("open")
        || model::bool_or(pr, "merged", false)
}

fn suite_window(config: &J, api: &dyn GitHubApi) -> Result<bool> {
    let sha = model::string(config, "sha").unwrap_or_default();
    let payload = api.get(&format!("commits/{sha}/check-suites?per_page=1"))?;
    let Some(count) = model::int(&payload, "total_count") else {
        return Err(watch("invalid Check suite count"));
    };
    if count < 0 {
        return Err(watch("invalid Check suite count"));
    }
    Ok(count <= 1000)
}

fn overflow() -> Vec<J> {
    vec![json!({
        "state": "needs_attention",
        "reason": "external_check_suite_window_exceeded",
        "decision_terminal": true
    })]
}

/// Check exact runs, also usable after landing when the PR is closed.
pub fn run_rows(config: &J, api: &dyn GitHubApi) -> Result<Vec<J>> {
    // The lane publication path supplies only `sha` plus the pinned check
    // inventory. Full PR identity is validated by `observe`, not here.
    let checks = validate_inventory(config)?;
    run_rows_for_checks(config, api, checks)
}

fn run_rows_for_checks(config: &J, api: &dyn GitHubApi, checks: Vec<J>) -> Result<Vec<J>> {
    suite_window(config, api).and_then(|within| run_rows_in_window(config, api, checks, within))
}

fn run_rows_in_window(
    config: &J,
    api: &dyn GitHubApi,
    checks: Vec<J>,
    within: bool,
) -> Result<Vec<J>> {
    if !within {
        return Ok(overflow());
    }
    collect_run_rows(config, api, checks)
}

fn collect_run_rows(config: &J, api: &dyn GitHubApi, checks: Vec<J>) -> Result<Vec<J>> {
    let sha = model::string(config, "sha").unwrap_or_default();
    fetch_check_runs(api, &sha)
        .and_then(|runs| build_check_rows(api, &sha, &checks, &runs))
        .and_then(|rows| finish_run_rows(config, api, rows))
}

fn fetch_check_runs(api: &dyn GitHubApi, sha: &str) -> Result<Vec<J>> {
    api.pages(
        &format!("commits/{sha}/check-runs?filter=all"),
        "check_runs",
    )
}

fn finish_run_rows(config: &J, api: &dyn GitHubApi, rows: Vec<J>) -> Result<Vec<J>> {
    suite_window(config, api).map(|within| finish_rows_within(within, rows))
}

fn finish_rows_within(within: bool, rows: Vec<J>) -> Vec<J> {
    if within {
        rows
    } else {
        overflow()
    }
}

fn build_check_rows(api: &dyn GitHubApi, sha: &str, checks: &[J], runs: &[J]) -> Result<Vec<J>> {
    let mut rows = Vec::new();
    build_remaining_check_rows(api, sha, checks, runs, &mut rows)?;
    Ok(rows)
}

fn build_remaining_check_rows(
    api: &dyn GitHubApi,
    sha: &str,
    checks: &[J],
    runs: &[J],
    rows: &mut Vec<J>,
) -> Result<()> {
    let Some((check, rest)) = checks.split_first() else {
        return Ok(());
    };
    check_row(api, sha, check, runs).and_then(|row| {
        rows.push(row);
        build_remaining_check_rows(api, sha, rest, runs, rows)
    })
}

fn check_row(api: &dyn GitHubApi, sha: &str, item: &J, runs: &[J]) -> Result<J> {
    let name = model::string(item, "name").unwrap_or_default();
    let app_id = model::int(item, "app_id").unwrap_or_default();
    let external_id = model::string(item, "external_id").unwrap_or_default();
    let mut matching = matching_check_runs(runs, &name, app_id);
    matching.sort_by_key(|run| model::int(run, "id").unwrap_or_default());
    let row = pending_check_row(&name, app_id, &external_id);
    let Some(run) = matching.last() else {
        return Ok(row);
    };
    let run_id = model::int(run, "id").unwrap_or_default();
    let current = api.get(&format!("check-runs/{run_id}"))?;
    finalize_check_row(row, current, sha, &name, app_id, &external_id, run_id)
}

fn matching_check_runs<'a>(runs: &'a [J], name: &str, app_id: i64) -> Vec<&'a J> {
    runs.iter()
        .filter(|run| {
            model::string(run, "name").as_deref() == Some(name)
                && model::dig_int(run, &["app", "id"]) == Some(app_id)
        })
        .collect()
}

fn pending_check_row(name: &str, app_id: i64, external_id: &str) -> J {
    json!({
        "name": name,
        "app_id": app_id,
        "external_id": external_id,
        "state": "pending",
        "reason": "external_check_missing",
        "decision_terminal": false
    })
}

fn identity_changed_check_row(name: &str, app_id: i64, external_id: &str) -> J {
    json!({
        "name": name,
        "app_id": app_id,
        "external_id": external_id,
        "state": "needs_attention",
        "reason": "external_check_identity_changed",
        "decision_terminal": true
    })
}

fn finalize_check_row(
    mut row: J,
    current: J,
    sha: &str,
    name: &str,
    app_id: i64,
    external_id: &str,
    run_id: i64,
) -> Result<J> {
    if check_identity_matches(&current, run_id, sha, name, app_id, external_id) {
        apply_check_status(&mut row, &current);
    } else {
        row = identity_changed_check_row(name, app_id, external_id);
    }
    Ok(row)
}

fn check_identity_matches(
    current: &J,
    run_id: i64,
    sha: &str,
    name: &str,
    app_id: i64,
    external_id: &str,
) -> bool {
    [
        model::int(current, "id") == Some(run_id),
        model::string(current, "name").as_deref() == Some(name),
        model::dig_int(current, &["app", "id"]) == Some(app_id),
        model::string(current, "head_sha").as_deref() == Some(sha),
        model::string(current, "external_id").as_deref() == Some(external_id),
    ]
    .into_iter()
    .all(|matches| matches)
}

fn apply_check_status(row: &mut J, current: &J) {
    let status = model::string(current, "status").unwrap_or_default();
    row["check_run"] = json!(current.get("id").cloned().unwrap_or(Value::Null));
    row["status"] = json!(status);
    row["conclusion"] = current.get("conclusion").cloned().unwrap_or(Value::Null);
    row["details_url"] = current.get("details_url").cloned().unwrap_or(Value::Null);
    row["decision_terminal"] = json!(status == "completed");
    apply_check_conclusion(row, current, &status);
}

fn apply_check_conclusion(row: &mut J, current: &J, status: &str) {
    if status != "completed" {
        row["reason"] = json!("external_check_pending");
        return;
    }
    let conclusion = model::string(current, "conclusion").unwrap_or_default();
    row["state"] = json!(conclusion_state(&conclusion));
    row["reason"] = json!(format!("external_check_{conclusion}"));
}

fn conclusion_state(conclusion: &str) -> &'static str {
    if conclusion == "success" {
        "success"
    } else {
        non_success_conclusion_state(conclusion)
    }
}

fn non_success_conclusion_state(conclusion: &str) -> &'static str {
    if conclusion == "failure" {
        "failure"
    } else {
        "needs_attention"
    }
}
