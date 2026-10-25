//! Read-only GitHub Actions observer.
//!
//! It writes a compact status plus deduplicated durable events and never
//! merges, reruns CI, edits repository settings, or changes branches.

use std::collections::BTreeSet;
use std::os::fd::RawFd;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use serde_json::{json, Map, Value};

use crate::ci::checks;
use crate::ci::policy;
use crate::error::{ErrorKind, FlowError, Result};
use crate::logging;
use crate::model::{self, J};
use crate::util;
use crate::util::process::{spawn_with_ready, terminate_child, BackgroundChild};
use crate::util::pyjson::{python_sorted_default, sha256_json_sorted, urlencode_query};

const MAX_JSON_SIZE: usize = 10 * 1024 * 1024;
const MAX_JSON_DEPTH: usize = 10;

pub fn watch_error(message: impl Into<String>) -> FlowError {
    FlowError::watch(message)
}

pub trait GitHubApi {
    fn get(&self, suffix: &str) -> Result<J>;

    fn pages(&self, suffix: &str, key: &str) -> Result<Vec<J>> {
        collect_pages(self, suffix, key, 1, Vec::new())
    }
}

fn collect_pages<A: GitHubApi + ?Sized>(
    api: &A,
    suffix: &str,
    key: &str,
    page: u32,
    result: Vec<J>,
) -> Result<Vec<J>> {
    let in_range = (1..=100).contains(&page);
    let Some(()) = in_range.then_some(()) else {
        return Err(watch_error(
            "pagination limit reached; completeness unknown",
        ));
    };
    page_chunk(api, suffix, key, page)
        .and_then(|chunk| collect_page_chunk(api, suffix, key, page, result, chunk))
}

fn collect_page_chunk<A: GitHubApi + ?Sized>(
    api: &A,
    suffix: &str,
    key: &str,
    page: u32,
    mut result: Vec<J>,
    chunk: Vec<J>,
) -> Result<Vec<J>> {
    let full = chunk.len() == 100;
    result.extend(chunk);
    if full {
        return collect_pages(api, suffix, key, page + 1, result);
    }
    Ok(result)
}

fn page_chunk<A: GitHubApi + ?Sized>(
    api: &A,
    suffix: &str,
    key: &str,
    page: u32,
) -> Result<Vec<J>> {
    let payload = api.get(&page_url(suffix, page))?;
    payload_chunk(payload, key)
}

fn page_url(suffix: &str, page: u32) -> String {
    let separator = if suffix.contains('?') { '&' } else { '?' };
    format!("{suffix}{separator}per_page=100&page={page}")
}

fn payload_chunk(payload: J, key: &str) -> Result<Vec<J>> {
    let Some(chunk) = model::get(&payload, key) else {
        return Err(watch_error("malformed paginated response"));
    };
    let Some(chunk) = chunk.as_array() else {
        return Err(watch_error("malformed paginated response"));
    };
    Ok(chunk.clone())
}

pub struct GitHub {
    config: J,
}

impl GitHub {
    pub fn new(config: J) -> Self {
        Self { config }
    }
}

impl GitHubApi for GitHub {
    fn get(&self, suffix: &str) -> Result<J> {
        let output = github_get_output(&self.config, suffix)?;
        safe_json_load(&output.stdout)
    }
}

fn github_get_output(config: &J, suffix: &str) -> Result<util::process::CommandOutput> {
    execute_github_get(config, suffix).and_then(require_github_get_success)
}

fn execute_github_get(config: &J, suffix: &str) -> Result<util::process::CommandOutput> {
    let argv = github_get_argv(config, suffix);
    let options = util::process::CommandOptions {
        timeout: Duration::from_secs(45),
        ..Default::default()
    };
    util::process::run_command(&argv, &options).map_err(map_github_get_error)
}

fn github_get_argv(config: &J, suffix: &str) -> Vec<String> {
    let host = model::string_or(config, "host", "github.com");
    let repo = model::string(config, "repo").unwrap_or_default();
    vec![
        "gh".to_string(),
        "api".to_string(),
        "--hostname".to_string(),
        host,
        "--method".to_string(),
        "GET".to_string(),
        format!("repos/{repo}/{suffix}"),
    ]
}

fn map_github_get_error(error: FlowError) -> FlowError {
    if error.kind == ErrorKind::Timeout {
        error
    } else {
        watch_error(format!("GitHub GET unavailable: {error}"))
    }
}

fn require_github_get_success(
    output: util::process::CommandOutput,
) -> Result<util::process::CommandOutput> {
    if output.status != 0 {
        return Err(watch_error(format!(
            "GitHub GET failed (exit {})",
            output.status
        )));
    }
    Ok(output)
}

/// Load JSON with size and depth validation to prevent denial of service.
pub fn safe_json_load(content: &[u8]) -> Result<J> {
    if content.len() > MAX_JSON_SIZE {
        return Err(watch_error(format!(
            "JSON content too large: {} bytes",
            content.len()
        )));
    }
    let data: J = serde_json::from_slice(content)
        .map_err(|error| watch_error(format!("invalid JSON: {error}")))?;
    check_depth(&data, 0)?;
    Ok(data)
}

fn check_depth(value: &J, depth: usize) -> Result<()> {
    if depth > MAX_JSON_DEPTH {
        return Err(watch_error("JSON nesting too deep"));
    }
    match value {
        Value::Object(map) => {
            for item in map.values() {
                check_depth(item, depth + 1)?;
            }
        }
        Value::Array(items) => {
            for item in items {
                check_depth(item, depth + 1)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn text_field(config: &J, key: &str, default: &str) -> Result<String> {
    match model::get(config, key) {
        None => Ok(default.to_string()),
        Some(Value::String(value)) => Ok(value.clone()),
        Some(_) => Err(watch_error(format!("{key} must be a string"))),
    }
}

fn validate_identity(config: &J) -> Result<()> {
    require_identity_object(config)
        .and_then(|()| require_identity_repo(config))
        .and_then(|()| require_identity_version(config))
        .and_then(|()| require_identity_sha(config))
        .and_then(|()| require_identity_host(config))
        .and_then(|()| require_identity_task(config))
        .and_then(|()| require_identity_epoch(config))
}

fn require_identity_object(config: &J) -> Result<()> {
    if config.is_object() {
        Ok(())
    } else {
        Err(watch_error("configuration must be a JSON object"))
    }
}

fn require_identity_version(config: &J) -> Result<()> {
    let version_ok = model::get(config, "version")
        .and_then(Value::as_f64)
        .map(|value| value == 1.0)
        .unwrap_or(false);
    if version_ok {
        Ok(())
    } else {
        Err(watch_error("version=1 and explicit owner/repo required"))
    }
}

fn require_identity_repo(config: &J) -> Result<()> {
    let repo = text_field(config, "repo", "")?;
    require_repo_name(&repo)
}

fn require_repo_name(repo: &str) -> Result<()> {
    if regex::Regex::new(r"^[\w.-]+/[\w.-]+$")
        .expect("static repo regex")
        .is_match(repo)
    {
        Ok(())
    } else {
        Err(watch_error("version=1 and explicit owner/repo required"))
    }
}

fn require_identity_sha(config: &J) -> Result<()> {
    let sha = text_field(config, "sha", "")?;
    require_full_sha(&sha)
}

fn require_full_sha(sha: &str) -> Result<()> {
    if model::is_full_sha(sha) {
        Ok(())
    } else {
        Err(watch_error("full 40-character SHA required"))
    }
}

fn require_identity_host(config: &J) -> Result<()> {
    let host = text_field(config, "host", "github.com")?;
    require_host_name(&host)
}

fn require_host_name(host: &str) -> Result<()> {
    if regex::Regex::new(r"^[A-Za-z0-9.-]+$")
        .expect("static host regex")
        .is_match(host)
    {
        Ok(())
    } else {
        Err(watch_error("invalid hostname"))
    }
}

fn require_identity_task(config: &J) -> Result<()> {
    let task_ok = model::get(config, "task")
        .and_then(Value::as_str)
        .map(|value| !value.is_empty())
        .unwrap_or(false);
    if task_ok {
        Ok(())
    } else {
        Err(watch_error("task and integer epoch required"))
    }
}

fn require_identity_epoch(config: &J) -> Result<()> {
    let epoch_ok = model::get(config, "epoch")
        .and_then(Value::as_i64)
        .is_some();
    if epoch_ok {
        Ok(())
    } else {
        Err(watch_error("task and integer epoch required"))
    }
}

fn validate_workflow(spec: &J) -> Result<(String, String)> {
    validate_workflow_object(spec)
        .and_then(|()| workflow_parts(spec))
        .and_then(|parts| validate_workflow_inventory(spec, parts))
}

fn validate_workflow_object(spec: &J) -> Result<()> {
    if spec.is_object() {
        Ok(())
    } else {
        Err(watch_error("each workflow entry must be an object"))
    }
}

fn workflow_parts(spec: &J) -> Result<(String, String)> {
    text_field(spec, "file", "")
        .and_then(|file| text_field(spec, "event", "").map(|event| (file, event)))
}

fn validate_workflow_inventory(spec: &J, parts: (String, String)) -> Result<(String, String)> {
    validate_workflow_names(&parts.0, &parts.1)
        .and_then(|()| workflow_jobs(spec))
        .and_then(|jobs| validate_workflow_decision(spec, &jobs))
        .map(|()| parts)
}

fn validate_workflow_names(file: &str, event: &str) -> Result<()> {
    require_workflow_file(file).and(require_workflow_event(event))
}

fn require_workflow_file(file: &str) -> Result<()> {
    if regex::Regex::new(r"^[\w.-]+\.ya?ml$")
        .expect("static workflow regex")
        .is_match(file)
    {
        Ok(())
    } else {
        Err(watch_error("workflow filename and event required"))
    }
}

fn require_workflow_event(event: &str) -> Result<()> {
    if regex::Regex::new(r"^[a-z_]+$")
        .expect("static event regex")
        .is_match(event)
    {
        Ok(())
    } else {
        Err(watch_error("workflow filename and event required"))
    }
}

fn workflow_jobs(spec: &J) -> Result<Vec<String>> {
    let Some(value) = model::get(spec, "jobs") else {
        return Ok(Vec::new());
    };
    let Some(items) = value.as_array() else {
        return Err(watch_error("jobs must be exact job names"));
    };
    job_name_strings(items)
}

fn job_name_strings(items: &[J]) -> Result<Vec<String>> {
    let mut jobs = Vec::new();
    for item in items {
        let Some(value) = item.as_str() else {
            return Err(watch_error("jobs must be exact job names"));
        };
        jobs.push(value.to_string());
    }
    Ok(jobs)
}

fn validate_workflow_decision(spec: &J, jobs: &[String]) -> Result<()> {
    let decision = match model::get(spec, "decision") {
        None => "workflow".to_string(),
        Some(value) => value
            .as_str()
            .map(ToString::to_string)
            .ok_or_else(|| watch_error("decision must be workflow or required_jobs"))?,
    };
    require_known_decision(&decision)?;
    require_required_job_shape(&decision, jobs)
}

fn require_known_decision(decision: &str) -> Result<()> {
    match decision {
        "workflow" | "required_jobs" => Ok(()),
        _ => Err(watch_error("decision must be workflow or required_jobs")),
    }
}

fn require_required_job_shape(decision: &str, jobs: &[String]) -> Result<()> {
    if decision != "required_jobs" {
        return Ok(());
    }
    require_jobs_nonempty(jobs)
        .and(require_exact_job_names(jobs))
        .and(require_unique_job_names(jobs))
}

fn require_jobs_nonempty(jobs: &[String]) -> Result<()> {
    if jobs.is_empty() {
        Err(watch_error(
            "required_jobs decision needs nonempty, unique exact job names",
        ))
    } else {
        Ok(())
    }
}

fn require_exact_job_names(jobs: &[String]) -> Result<()> {
    if jobs
        .iter()
        .all(|job| !job.trim().is_empty() && job.trim() == job)
    {
        Ok(())
    } else {
        Err(watch_error(
            "required_jobs decision needs nonempty, unique exact job names",
        ))
    }
}

fn require_unique_job_names(jobs: &[String]) -> Result<()> {
    let unique: BTreeSet<&str> = jobs.iter().map(String::as_str).collect();
    if unique.len() == jobs.len() {
        Ok(())
    } else {
        Err(watch_error(
            "required_jobs decision needs nonempty, unique exact job names",
        ))
    }
}

fn number_in_range(config: &J, key: &str, default: f64, low: f64, high: f64) -> Result<f64> {
    let Some(value) = model::get(config, key) else {
        return Ok(default);
    };
    let Some(number) = value.as_f64() else {
        return Err(watch_error(format!("invalid {key}")));
    };
    require_number_in_range(number, key, low, high)
}

fn require_number_in_range(value: f64, key: &str, low: f64, high: f64) -> Result<f64> {
    if within_range(value, low, high) {
        Ok(value)
    } else {
        Err(watch_error(format!("invalid {key}")))
    }
}

fn within_range(value: f64, low: f64, high: f64) -> bool {
    (low..=high).contains(&value) && value.is_finite()
}

fn validate_ranges(config: &J) -> Result<()> {
    if let Some(pr) = model::get(config, "pr") {
        if !pr.is_null() {
            match pr.as_i64() {
                Some(value) if value >= 1 => {}
                _ => return Err(watch_error("invalid PR number")),
            }
        }
    }
    number_in_range(config, "interval", 60.0, 10.0, 3600.0)?;
    number_in_range(config, "deadline_seconds", 21600.0, 1.0, 604_800.0)?;
    Ok(())
}

pub fn validate(config: &J) -> Result<J> {
    validate_identity(config)?;
    validate_inventory(config)
}

fn validate_inventory(config: &J) -> Result<J> {
    let specs = workflow_specs(config)?;
    validate_explicit_inventory(config, specs)
}

fn validate_explicit_inventory(config: &J, specs: Vec<J>) -> Result<J> {
    checks::validate(config)
        .and_then(|checks| validate_presence(&specs, &checks))
        .and_then(|()| validate_workflow_keys(&specs))
        .and_then(|()| validate_ranges(config))
        .map(|()| config.clone())
}

fn workflow_specs(config: &J) -> Result<Vec<J>> {
    let Some(value) = model::get(config, "workflows") else {
        return Ok(Vec::new());
    };
    let Some(items) = value.as_array() else {
        return Err(watch_error(
            "explicit nonempty workflow/event or external Check inventory required",
        ));
    };
    Ok(items.clone())
}

fn validate_presence(specs: &[J], checks: &[J]) -> Result<()> {
    if checks.is_empty() {
        return require_workflow_specs(specs);
    }
    Ok(())
}

fn require_workflow_specs(specs: &[J]) -> Result<()> {
    if specs.is_empty() {
        Err(watch_error(
            "explicit nonempty workflow/event or external Check inventory required",
        ))
    } else {
        Ok(())
    }
}

fn validate_workflow_keys(specs: &[J]) -> Result<()> {
    let mut keys = BTreeSet::new();
    register_workflow_specs(specs, &mut keys)
}

fn register_workflow_specs(specs: &[J], keys: &mut BTreeSet<(String, String)>) -> Result<()> {
    let Some((spec, rest)) = specs.split_first() else {
        return Ok(());
    };
    validate_workflow(spec)
        .and_then(|key| register_workflow_key(keys, key))
        .and_then(|()| register_workflow_specs(rest, keys))
}

fn register_workflow_key(
    keys: &mut BTreeSet<(String, String)>,
    key: (String, String),
) -> Result<()> {
    if keys.insert(key) {
        Ok(())
    } else {
        Err(watch_error("duplicate workflow/event"))
    }
}

fn superseded(config: &J, api: &dyn GitHubApi) -> Result<Option<J>> {
    let Some(pr) = model::get(config, "pr") else {
        return Ok(None);
    };
    if pr.is_null() {
        return Ok(None);
    }
    let number = pr.as_i64().unwrap_or_default();
    let pull = api.get(&format!("pulls/{number}"))?;
    let sha = model::string(config, "sha").unwrap_or_default();
    if model::dig_str(&pull, &["head", "sha"]) != Some(sha) {
        return Ok(Some(json!({
            "state": "superseded",
            "reason": "PR head changed",
            "workflows": []
        })));
    }
    Ok(None)
}

fn run_state(
    run: &J,
    jobs: &[J],
    spec: &J,
    missing: &[String],
    bad: bool,
    required_bad: bool,
) -> &'static str {
    if bad
        || required_bad
        || (model::string(run, "status").as_deref() == Some("completed")
            && model::string(run, "conclusion").as_deref() != Some("success"))
    {
        return "failure";
    }
    if model::string(run, "status").as_deref() != Some("completed") {
        return "running";
    }
    let required: BTreeSet<String> = model::array_field(spec, "jobs")
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(ToString::to_string)
                .collect()
        })
        .unwrap_or_default();
    let _ = required;
    if !jobs.is_empty()
        && missing.is_empty()
        && jobs
            .iter()
            .all(|job| model::string(job, "status").as_deref() == Some("completed"))
    {
        "success"
    } else {
        "failure"
    }
}

fn workflow_row(config: &J, spec: &J, api: &dyn GitHubApi) -> Result<J> {
    let sha = model::string(config, "sha").unwrap_or_default();
    let event = model::string(spec, "event").unwrap_or_default();
    let file = model::string(spec, "file").unwrap_or_default();
    workflow_runs(&sha, &event, &file, api)
        .and_then(|runs| workflow_row_for_runs(spec, api, &sha, &event, &file, runs))
}

fn workflow_runs(sha: &str, event: &str, file: &str, api: &dyn GitHubApi) -> Result<Vec<J>> {
    let query = urlencode_query(&[("head_sha", sha), ("event", event)]);
    api.pages(
        &format!("actions/workflows/{file}/runs?{query}"),
        "workflow_runs",
    )
}

fn workflow_row_for_runs(
    spec: &J,
    api: &dyn GitHubApi,
    sha: &str,
    event: &str,
    file: &str,
    runs: Vec<J>,
) -> Result<J> {
    let matching = matching_workflow_runs(&runs, sha, event);
    let row = workflow_stub_row(file, event);
    let Some(run) = latest_workflow_run(&matching) else {
        return Ok(row);
    };
    workflow_row_current(spec, api, sha, run, row)
}

fn matching_workflow_runs<'a>(runs: &'a [J], sha: &str, event: &str) -> Vec<&'a J> {
    runs.iter()
        .filter(|run| {
            model::string(run, "head_sha").as_deref() == Some(sha)
                && model::string(run, "event").as_deref() == Some(event)
        })
        .collect()
}

fn latest_workflow_run<'a>(matching: &[&'a J]) -> Option<&'a J> {
    matching
        .iter()
        .copied()
        .max_by_key(|run| model::int(run, "id").unwrap_or_default())
}

fn workflow_stub_row(file: &str, event: &str) -> J {
    json!({
        "file": file,
        "event": event,
        "state": "missing"
    })
}

fn workflow_row_current(spec: &J, api: &dyn GitHubApi, sha: &str, run: &J, row: J) -> Result<J> {
    let run_id = model::int(run, "id").unwrap_or_default();
    let attempt = model::int(run, "run_attempt").unwrap_or_default();
    workflow_run_jobs(api, run_id, attempt)
        .and_then(|jobs| validate_workflow_job_identities(&jobs, run_id, sha).map(|()| jobs))
        .and_then(|jobs| {
            current_workflow_run(api, run_id, attempt, sha).map(|current| (jobs, current))
        })
        .and_then(|(jobs, current)| workflow_final_row(spec, attempt, row, jobs, current))
}

fn workflow_run_jobs(api: &dyn GitHubApi, run_id: i64, attempt: i64) -> Result<Vec<J>> {
    api.pages(
        &format!("actions/runs/{run_id}/attempts/{attempt}/jobs"),
        "jobs",
    )
}

fn validate_workflow_job_identities(jobs: &[J], run_id: i64, sha: &str) -> Result<()> {
    let mismatch = jobs.iter().any(|job| {
        model::int(job, "run_id") != Some(run_id)
            || model::string(job, "head_sha").unwrap_or_else(|| sha.to_string()) != sha
    });
    if mismatch {
        Err(watch_error("job identity mismatch"))
    } else {
        Ok(())
    }
}

fn current_workflow_run(api: &dyn GitHubApi, run_id: i64, attempt: i64, sha: &str) -> Result<J> {
    let current = api.get(&format!("actions/runs/{run_id}"))?;
    validate_current_workflow_run(&current, attempt, sha).map(|()| current)
}

fn validate_current_workflow_run(current: &J, attempt: i64, sha: &str) -> Result<()> {
    require_run_attempt(current, attempt).and(require_run_head_sha(current, sha))
}

fn require_run_attempt(current: &J, attempt: i64) -> Result<()> {
    if model::int(current, "run_attempt") == Some(attempt) {
        Ok(())
    } else {
        Err(watch_error("run changed during observation"))
    }
}

fn require_run_head_sha(current: &J, sha: &str) -> Result<()> {
    if model::string(current, "head_sha").as_deref() == Some(sha) {
        Ok(())
    } else {
        Err(watch_error("run changed during observation"))
    }
}

fn workflow_final_row(spec: &J, attempt: i64, row: J, jobs: Vec<J>, current: J) -> Result<J> {
    let names = job_name_set(&jobs);
    let required = required_job_set(spec);
    let mut missing: Vec<String> = required.difference(&names).cloned().collect();
    missing.sort();
    let bad = has_bad_job(&jobs);
    let required_bad = has_bad_required_job(&jobs, &required);
    let state = run_state(&current, &jobs, spec, &missing, bad, required_bad);
    let row = completed_workflow_row(row, &current, attempt, &missing, &jobs, state);
    apply_workflow_policy(spec, &current, &jobs, row)
}

fn job_name_set(jobs: &[J]) -> BTreeSet<String> {
    jobs.iter()
        .filter_map(|job| model::string(job, "name"))
        .collect()
}

fn required_job_set(spec: &J) -> BTreeSet<String> {
    model::array_field(spec, "jobs")
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(ToString::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn has_bad_job(jobs: &[J]) -> bool {
    jobs.iter().any(|job| {
        model::string(job, "status").as_deref() == Some("completed")
            && !matches!(
                model::string(job, "conclusion").as_deref(),
                Some("success" | "skipped" | "neutral")
            )
    })
}

fn has_bad_required_job(jobs: &[J], required: &BTreeSet<String>) -> bool {
    jobs.iter().any(|job| {
        model::string(job, "name")
            .map(|name| required.contains(&name))
            .unwrap_or(false)
            && model::string(job, "status").as_deref() == Some("completed")
            && model::string(job, "conclusion").as_deref() != Some("success")
    })
}

fn completed_workflow_row(
    mut row: J,
    current: &J,
    attempt: i64,
    missing: &[String],
    jobs: &[J],
    state: &'static str,
) -> J {
    row["state"] = json!(state);
    row["run_status"] = current.get("status").cloned().unwrap_or(Value::Null);
    row["run_id"] = current.get("id").cloned().unwrap_or(Value::Null);
    row["attempt"] = json!(attempt);
    row["url"] = current.get("html_url").cloned().unwrap_or(Value::Null);
    row["conclusion"] = current.get("conclusion").cloned().unwrap_or(Value::Null);
    row["missing_jobs"] = json!(missing);
    row["jobs"] = Value::Array(jobs.iter().map(workflow_job_row).collect());
    row
}

fn workflow_job_row(job: &J) -> J {
    json!({
        "id": job.get("id").cloned().unwrap_or(Value::Null),
        "name": job.get("name").cloned().unwrap_or(Value::Null),
        "status": job.get("status").cloned().unwrap_or(Value::Null),
        "conclusion": job.get("conclusion").cloned().unwrap_or(Value::Null),
        "html_url": job.get("html_url").cloned().unwrap_or(Value::Null)
    })
}

fn apply_workflow_policy(spec: &J, run: &J, jobs: &[J], row: J) -> Result<J> {
    if model::string(spec, "decision").as_deref() != Some("required_jobs") {
        return Ok(row);
    }
    apply_required_jobs_policy(spec, run, jobs, row)
}

fn apply_required_jobs_policy(spec: &J, run: &J, jobs: &[J], mut row: J) -> Result<J> {
    let policy = policy::required_jobs(spec, run, jobs)?;
    merge_policy_fields(&mut row, &policy);
    row["decision"] = json!("required_jobs");
    Ok(row)
}

fn merge_policy_fields(row: &mut J, policy: &J) {
    let (Some(target), Some(source)) = (row.as_object_mut(), policy.as_object()) else {
        return;
    };
    for (key, value) in source {
        target.insert(key.clone(), value.clone());
    }
}

fn overall_state(rows: &[J], checks: &[J]) -> &'static str {
    let mut states = BTreeSet::new();
    for row in rows.iter().chain(checks.iter()) {
        if let Some(state) = model::string(row, "state") {
            states.insert(state);
        }
    }
    if states.contains("needs_attention") {
        return "needs_attention";
    }
    if states.contains("failure") {
        return "failure";
    }
    if states.len() == 1 && states.contains("success") {
        return "success";
    }
    "pending"
}

pub fn observe(config: &J, api: &dyn GitHubApi) -> Result<J> {
    if let Some(result) = superseded(config, api)? {
        return Ok(result);
    }
    let specs = model::array_field(config, "workflows")
        .cloned()
        .unwrap_or_default();
    let mut rows = Vec::new();
    for spec in &specs {
        rows.push(workflow_row(config, spec, api)?);
    }
    let check_rows = checks::observe(config, api)?;
    let state = overall_state(&rows, &check_rows);
    let mut result = json!({"state": state, "workflows": rows});
    if !check_rows.is_empty() {
        result["checks"] = Value::Array(check_rows);
    }
    Ok(result)
}

pub fn persist(root: &Path, config_hash: &str, result: &J) -> Result<J> {
    require_unbound_observer(root)?;
    util::ensure_private_dir(root, true)?;
    let key = sha256_json_sorted(result);
    let mut event = Map::new();
    event.insert("id".to_string(), json!(key));
    event.insert("config_hash".to_string(), json!(config_hash));
    event.insert("observed_at".to_string(), json!(util::now()));
    if let Some(map) = result.as_object() {
        for (name, value) in map {
            event.insert(name.clone(), value.clone());
        }
    }
    let event = Value::Object(event);
    let target = root.join("events").join(format!("{key}.json"));
    if !target.exists() {
        util::atomic_write_json(&target, &event)?;
    }
    util::atomic_write_json(&root.join("status.json"), &event)?;
    Ok(event)
}

struct ReadyNotifier {
    fd: Option<RawFd>,
}

impl ReadyNotifier {
    fn new(fd: Option<RawFd>) -> Self {
        Self { fd }
    }

    fn notify(&mut self, payload: J) {
        if let Some(fd) = self.fd.take() {
            util::write_ready_fd(fd, &payload);
        }
    }
}

pub fn run(config_path: &Path, root: &Path, once: bool, ready_fd: Option<RawFd>) -> Result<i32> {
    require_unbound_observer(root)?;
    let mut notifier = ReadyNotifier::new(ready_fd);
    observe_loop(config_path, root, once, &mut notifier).map_err(|error| {
        notifier.notify(json!({
            "ready": false,
            "error": format!("{}: {}", error.kind.type_name(), error)
        }));
        error
    })
}

fn observer_identity(root: &Path, config_hash: &str, config: &J) -> Result<J> {
    let identity_path = root.join("identity.json");
    if identity_path.exists() {
        return existing_observer_identity(&identity_path, config_hash);
    }
    write_observer_identity(&identity_path, config_hash, config)
}

fn existing_observer_identity(identity_path: &Path, config_hash: &str) -> Result<J> {
    // observer_identity always constructs this path with root.join("identity.json").
    let root = identity_path.parent().expect("observer identity parent");
    let meta = util::read_state_json(root, "identity.json")?;
    require_observer_identity(&meta, config_hash).map(|()| meta)
}

fn require_observer_identity(meta: &J, config_hash: &str) -> Result<()> {
    if model::string(meta, "config_hash").as_deref() == Some(config_hash) {
        Ok(())
    } else {
        Err(watch_error(
            "state directory belongs to another configuration; use a new epoch directory",
        ))
    }
}

fn write_observer_identity(identity_path: &Path, config_hash: &str, config: &J) -> Result<J> {
    let meta = json!({
        "config_hash": config_hash,
        "started_at": util::now(),
        "config": config
    });
    util::atomic_write_json(identity_path, &meta)?;
    Ok(meta)
}

fn observation(config: &J, api: &dyn GitHubApi, errors: i64) -> (J, i64) {
    observe(config, api)
        .map(|result| (result, errors))
        .unwrap_or_else(|error| observation_error(config, error, errors))
}

fn observation_error(config: &J, error: FlowError, errors: i64) -> (J, i64) {
    let errors = errors + 1;
    let message = observation_error_message(&error);
    let result = json!({
        "state": "api_error",
        "error": message,
        "workflows": []
    });
    log_observation_error(config, &message, errors);
    (result, errors)
}

fn observation_error_message(error: &FlowError) -> String {
    if error.kind == ErrorKind::Watch {
        error.message.clone()
    } else {
        error.kind.type_name().to_string()
    }
}

fn log_observation_error(config: &J, message: &str, errors: i64) {
    let metadata = json!({
        "error_count": errors,
        "repo": model::string(config, "repo")
    });
    logging::log_event_fields(
        "ci_observation_error",
        "ci_watch",
        &format!("CI observation error: {message}"),
        logging::Fields {
            metadata: Some(&metadata),
            ..Default::default()
        },
    );
}

fn workflow_terminal(row: &J) -> bool {
    if model::string(row, "decision").as_deref() == Some("required_jobs") {
        return model::bool_or(row, "decision_terminal", false);
    }
    successful_or_failed_run(row)
}

fn successful_or_failed_run(row: &J) -> bool {
    terminal_workflow_state(row) && run_and_jobs_terminal(row)
}

fn terminal_workflow_state(row: &J) -> bool {
    matches!(
        model::string(row, "state").as_deref(),
        Some("success" | "failure")
    )
}

fn run_and_jobs_terminal(row: &J) -> bool {
    model::string(row, "run_status").as_deref() == Some("completed") && jobs_all_completed(row)
}

fn jobs_all_completed(row: &J) -> bool {
    model::array_field(row, "jobs")
        .map(|jobs| {
            jobs.iter()
                .all(|job| model::string(job, "status").as_deref() == Some("completed"))
        })
        .unwrap_or(true)
}

fn all_terminal(result: &J) -> bool {
    model::array_field(result, "workflows")
        .map(|rows| rows.iter().all(workflow_terminal))
        .unwrap_or(true)
        && model::array_field(result, "checks")
            .map(|rows| {
                rows.iter()
                    .all(|row| model::bool_or(row, "decision_terminal", false))
            })
            .unwrap_or(true)
}

pub fn result_code(state: &str) -> i32 {
    let known = [
        ("success", 0),
        ("failure", 1),
        ("superseded", 2),
        ("api_error", 2),
        ("needs_attention", 2),
    ];
    known
        .iter()
        .find(|(name, _)| *name == state)
        .map(|(_, code)| *code)
        .unwrap_or(3)
}

fn observe_loop(
    config_path: &Path,
    root: &Path,
    once: bool,
    notifier: &mut ReadyNotifier,
) -> Result<i32> {
    load_watch_config(config_path)
        .and_then(|config| WatchState::start(config, root, notifier))
        .and_then(|mut state| state.run_loop(once))
}

fn load_watch_config(config_path: &Path) -> Result<J> {
    validate(&util::read_json(config_path)?)
}

struct WatchState {
    config: J,
    config_hash: String,
    root: PathBuf,
    api: GitHub,
    deadline_seconds: f64,
    interval: f64,
    started_at: f64,
    errors: i64,
    _lock: util::FileLock,
}

impl WatchState {
    fn start(config: J, root: &Path, notifier: &mut ReadyNotifier) -> Result<Self> {
        let config_hash = sha256_json_sorted(&config);
        prepare_watch_root(root)
            .and_then(|root| acquire_watch_lock(&root).map(|lock| (root, lock)))
            .and_then(|(root, lock)| ready_watch_state(config, config_hash, root, lock, notifier))
    }

    fn run_loop(&mut self, once: bool) -> Result<i32> {
        loop {
            let outcome = self.iterate(once);
            let Ok(outcome) = outcome else {
                // Propagate persistence errors to the caller.
                return outcome.map(|_| 0);
            };
            let Some(code) = outcome else {
                self.backoff();
                continue;
            };
            return Ok(code);
        }
    }

    fn iterate(&mut self, once: bool) -> Result<Option<i32>> {
        if self.timed_out() {
            return self.timeout();
        }
        self.observe_once(once)
    }

    fn timed_out(&self) -> bool {
        util::now() - self.started_at >= self.deadline_seconds
    }

    fn timeout(&mut self) -> Result<Option<i32>> {
        persist(
            &self.root,
            &self.config_hash,
            &json!({"state": "timeout", "workflows": []}),
        )
        .map(|_| Some(2))
    }

    fn observe_once(&mut self, once: bool) -> Result<Option<i32>> {
        let (result, next_errors) = observation(&self.config, &self.api, self.errors);
        self.errors = next_errors;
        persist(&self.root, &self.config_hash, &result).map(|_| exit_code(&result, once))
    }

    fn backoff(&self) {
        let exponent = self.errors.min(4) as u32;
        let delay = (self.interval * 2f64.powi(exponent as i32)).min(900.0);
        thread::sleep(Duration::from_secs_f64(delay.max(0.001)));
    }
}

fn prepare_watch_root(root: &Path) -> Result<PathBuf> {
    let root = util::state_dir(root, true)?;
    Ok(root)
}

fn acquire_watch_lock(root: &Path) -> Result<util::FileLock> {
    let lock = util::FileLock::acquire(&root.join("watch.lock"), false)?;
    let Some(lock) = lock else {
        return Err(watch_error("another observer owns this state directory"));
    };
    Ok(lock)
}

fn ready_watch_state(
    config: J,
    config_hash: String,
    root: PathBuf,
    lock: util::FileLock,
    notifier: &mut ReadyNotifier,
) -> Result<WatchState> {
    let meta = observer_identity(&root, &config_hash, &config)?;
    // Ready means this exact process owns the lock and the identity matches.
    notifier.notify(json!({"ready": true, "pid": std::process::id()}));
    Ok(watch_state_from_parts(
        config,
        config_hash,
        root,
        lock,
        &meta,
    ))
}

fn watch_state_from_parts(
    config: J,
    config_hash: String,
    root: PathBuf,
    lock: util::FileLock,
    meta: &J,
) -> WatchState {
    let deadline_seconds = model::number(&config, "deadline_seconds").unwrap_or(21600.0);
    let interval = model::number(&config, "interval").unwrap_or(60.0);
    let started_at = model::number(meta, "started_at").unwrap_or_else(util::now);
    WatchState {
        api: GitHub::new(config.clone()),
        config,
        config_hash,
        root,
        deadline_seconds,
        interval,
        started_at,
        errors: 0,
        _lock: lock,
    }
}

fn exit_code(result: &J, once: bool) -> Option<i32> {
    let state = model::string(result, "state").unwrap_or_default();
    if once {
        return Some(result_code(&state));
    }
    early_success_code(&state, result)
}

fn early_success_code(state: &str, result: &J) -> Option<i32> {
    if matches!(state, "success" | "superseded") {
        return Some(result_code(state));
    }
    terminal_exit_code(state, result)
}

fn terminal_exit_code(state: &str, result: &J) -> Option<i32> {
    if !attention_or_failure(state) {
        return None;
    }
    // Keep observing after early failures so remaining jobs can finish.
    all_terminal(result).then(|| final_result_code(state))
}

fn attention_or_failure(state: &str) -> bool {
    matches!(state, "failure" | "needs_attention")
}

fn final_result_code(state: &str) -> i32 {
    if state == "failure" {
        1
    } else {
        2
    }
}

pub fn background(config_path: &Path, state_dir: &Path) -> Result<J> {
    validate_background_config(config_path)
        .and_then(|()| prepare_background_root(state_dir))
        .and_then(|root| launch_background_observer(config_path, root))
}

fn validate_background_config(config_path: &Path) -> Result<()> {
    validate(&util::read_json(config_path)?).map(|_| ())
}

fn prepare_background_root(state_dir: &Path) -> Result<PathBuf> {
    let root = util::state_dir(state_dir, true)?;
    Ok(root)
}

fn launch_background_observer(config_path: &Path, root: PathBuf) -> Result<J> {
    spawn_background_child(config_path, &root)
        .and_then(|mut child| confirm_background_ready(&mut child).map(|()| child))
        .map(|child| background_result(child, &root))
}

fn spawn_background_child(config_path: &Path, root: &Path) -> Result<BackgroundChild> {
    let exe = util::current_exe()?;
    let argv = background_observer_argv(exe, config_path, root);
    spawn_with_ready(
        &argv,
        &root.join("process.log"),
        None,
        Duration::from_secs(15),
    )
}

fn background_observer_argv(exe: PathBuf, config_path: &Path, root: &Path) -> Vec<String> {
    vec![
        exe.display().to_string(),
        "ci".to_string(),
        "--config".to_string(),
        util::resolve_path(config_path).display().to_string(),
        "--state-dir".to_string(),
        root.display().to_string(),
    ]
}

fn confirm_background_ready(child: &mut BackgroundChild) -> Result<()> {
    let ready = child.ready.clone().unwrap_or(Value::Null);
    if !model::bool_or(&ready, "ready", false) {
        return reject_background_child(child, &ready);
    }
    let ready_pid = model::int(&ready, "pid").unwrap_or_default();
    let acceptable = acceptable_background_pid(ready_pid, child.pid);
    let Some(()) = acceptable.then_some(()) else {
        return reject_background_child(child, &ready);
    };
    Ok(())
}

fn acceptable_background_pid(ready_pid: i64, pid: u32) -> bool {
    ready_pid == pid as i64
}

fn reject_background_child<T>(child: &mut BackgroundChild, ready: &J) -> Result<T> {
    let message = model::string(ready, "error")
        .unwrap_or_else(|| "observer exited before readiness; inspect process.log".to_string());
    terminate_child(&mut child.child);
    Err(watch_error(message))
}

fn background_result(child: BackgroundChild, root: &Path) -> J {
    let pid = child.pid;
    child.reap();
    json!({"pid": pid, "state_dir": root.display().to_string(), "ready": true})
}

fn parse_value(args: &[String], index: &mut usize) -> Result<String> {
    *index += 1;
    args.get(*index)
        .cloned()
        .ok_or_else(|| FlowError::usage("missing option value"))
}

#[derive(Default)]
struct CliOptions {
    config: Option<PathBuf>,
    state_dir: Option<PathBuf>,
    once: bool,
    background_mode: bool,
    ready_fd: Option<RawFd>,
}

#[derive(Clone, Copy)]
enum CliStep {
    Continue(usize),
    Help,
}

type CliApply = fn(&mut CliOptions, &[String], usize) -> Result<CliStep>;

const CLI_OPTIONS: &[(&str, CliApply)] = &[
    ("--config", apply_cli_config),
    ("--state-dir", apply_cli_state_dir),
    ("--once", apply_cli_once),
    ("--background", apply_cli_background),
    ("--ready-fd", apply_cli_ready_fd),
    ("-h", apply_cli_help),
    ("--help", apply_cli_help),
];

pub fn cli(args: &[String]) -> i32 {
    parse_cli(args)
        .map(|options| options.map_or(0, dispatch_cli))
        .unwrap_or_else(cli_error)
}

fn parse_cli(args: &[String]) -> Result<Option<CliOptions>> {
    parse_cli_at(args, 0, CliOptions::default())
}

fn parse_cli_at(
    args: &[String],
    index: usize,
    mut options: CliOptions,
) -> Result<Option<CliOptions>> {
    let Some(argument) = args.get(index) else {
        return Ok(Some(options));
    };
    let step = parse_cli_arg(args, index, argument, &mut options)?;
    advance_cli(args, step, options)
}

fn advance_cli(args: &[String], step: CliStep, options: CliOptions) -> Result<Option<CliOptions>> {
    let CliStep::Continue(next) = step else {
        return Ok(None);
    };
    parse_cli_at(args, next, options)
}

fn parse_cli_arg(
    args: &[String],
    index: usize,
    argument: &str,
    options: &mut CliOptions,
) -> Result<CliStep> {
    let Some((_, apply)) = CLI_OPTIONS
        .iter()
        .find(|(name, _)| *name == argument)
        .copied()
    else {
        return Err(FlowError::usage(format!(
            "unrecognized argument: {argument}"
        )));
    };
    apply(options, args, index)
}

fn option_value(args: &[String], index: usize) -> Result<(String, usize)> {
    let mut cursor = index;
    parse_value(args, &mut cursor).map(|value| (value, cursor + 1))
}

fn apply_cli_config(options: &mut CliOptions, args: &[String], index: usize) -> Result<CliStep> {
    let (value, next) = option_value(args, index)?;
    options.config = Some(PathBuf::from(value));
    Ok(CliStep::Continue(next))
}

fn apply_cli_state_dir(options: &mut CliOptions, args: &[String], index: usize) -> Result<CliStep> {
    let (value, next) = option_value(args, index)?;
    options.state_dir = Some(PathBuf::from(value));
    Ok(CliStep::Continue(next))
}

fn apply_cli_once(options: &mut CliOptions, _args: &[String], index: usize) -> Result<CliStep> {
    options.once = true;
    Ok(CliStep::Continue(index + 1))
}

fn apply_cli_background(
    options: &mut CliOptions,
    _args: &[String],
    index: usize,
) -> Result<CliStep> {
    options.background_mode = true;
    Ok(CliStep::Continue(index + 1))
}

fn apply_cli_ready_fd(options: &mut CliOptions, args: &[String], index: usize) -> Result<CliStep> {
    option_value(args, index)
        .and_then(|(value, next)| parse_ready_fd(&value).map(|fd| (fd, next)))
        .map(|(fd, next)| {
            options.ready_fd = Some(fd);
            CliStep::Continue(next)
        })
}

fn parse_ready_fd(value: &str) -> Result<RawFd> {
    value
        .parse::<RawFd>()
        .map_err(|_| FlowError::usage("invalid --ready-fd"))
}

fn apply_cli_help(_options: &mut CliOptions, _args: &[String], _index: usize) -> Result<CliStep> {
    println!("Usage: herdr-flow ci --config FILE --state-dir DIR [--once] [--background]");
    Ok(CliStep::Help)
}

fn dispatch_cli(mut options: CliOptions) -> i32 {
    let (Some(config), Some(state_dir)) = (options.config.take(), options.state_dir.take()) else {
        return cli_error(FlowError::usage("--config and --state-dir are required"));
    };
    run_cli_mode(config, state_dir, options)
}

fn run_cli_mode(config: PathBuf, state_dir: PathBuf, options: CliOptions) -> i32 {
    if let Err(error) = require_unbound_observer(&state_dir) {
        eprintln!("{error}");
        return 2;
    }
    if options.background_mode {
        return start_background_cli(config, state_dir, options.once);
    }
    run_foreground_cli(&config, &state_dir, options.once, options.ready_fd)
}

fn start_background_cli(config: PathBuf, state_dir: PathBuf, once: bool) -> i32 {
    if once {
        return cli_error(watch_error(
            "--background and --once are mutually exclusive",
        ));
    }
    background_cli(&config, &state_dir)
}

fn background_cli(config: &Path, state_dir: &Path) -> i32 {
    background(config, state_dir).map_or_else(cli_error, |value| {
        println!("{value}");
        0
    })
}

fn run_foreground_cli(config: &Path, state_dir: &Path, once: bool, ready_fd: Option<RawFd>) -> i32 {
    run(config, state_dir, once, ready_fd).unwrap_or_else(cli_error)
}

fn cli_error(error: FlowError) -> i32 {
    util::write_stderr(&format!("{}: {}\n", error.kind.type_name(), error));
    2
}

/// The canonical configuration hash used by queue/PR state identities.
pub fn config_hash(config: &J) -> String {
    util::sha256_hex(python_sorted_default(config).as_bytes())
}

/// CI observation has no task-owned observer lifecycle registration in this slice.
/// Refuse rather than run an unbound writer under an isolated task's authority.
fn require_unbound_observer(root: &Path) -> Result<()> {
    if std::env::var_os("HERDR_LIFECYCLE_OWNER").is_some()
        || util::protected::is_runtime_namespace(root)?
        || root.join("lifecycle-owner.json").exists()
    {
        return Err(watch_error(
            "task-exclusive CI watcher lifecycle is unsupported; use bounded queue observation",
        ));
    }
    Ok(())
}
