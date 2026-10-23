//! V1 PR provenance and reconciliation inside `queue.json`'s transaction.
//!
//! Only GETs and local Git reads happen here. Publication and repair transport
//! use saved intents; an uncertain operation is never automatically replayed.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Map, Value};

use crate::batch::validation::publication_check;
use crate::ci::{checks as check_policy, GitHub, GitHubApi};
use crate::error::{FlowError, Result};
use crate::model::{self, J};
use crate::progress;
use crate::util;
use crate::util::pyjson::{sha256_json_sorted, urlencode_query};

struct RepairEvidence<'a> {
    expected: &'a J,
    evidence: &'a str,
    evidence_path: &'a Path,
}

struct ObservationState {
    working: J,
    source: PathBuf,
    result: J,
    digest: String,
}

struct WorkflowIdentity<'a> {
    file: &'a str,
    event: &'a str,
}

struct PullRequestState {
    task: J,
    record: J,
}

struct PreparationPolicy {
    issue: i64,
    max_repairs: i64,
}

struct RepositoryIdentity {
    repo: String,
    host: String,
}

pub fn sha(value: Option<&J>) -> Result<String> {
    let value = value.and_then(Value::as_str).unwrap_or_default();
    if !model::is_full_sha(value) {
        return Err(FlowError::progress("PR provenance requires full SHAs"));
    }
    Ok(value.to_string())
}

pub fn git(cwd: &Path, args: &[&str]) -> Result<String> {
    let mut argv = vec![
        "git".to_string(),
        "-C".to_string(),
        cwd.display().to_string(),
    ];
    argv.extend(args.iter().map(|argument| argument.to_string()));
    let options = util::process::git_options(Duration::from_secs(15));
    let output = util::process::run_command(&argv, &options).map_err(|error| {
        if error.kind == crate::error::ErrorKind::Timeout {
            FlowError::progress("local Git provenance query timed out")
        } else {
            FlowError::progress("local Git provenance could not be verified")
        }
    })?;
    if output.status != 0 {
        return Err(FlowError::progress(
            "local Git provenance could not be verified",
        ));
    }
    Ok(output.stdout_text().trim().to_string())
}

fn provenance_checkout(spec: &J) -> Result<PathBuf> {
    let checkout = progress::text(model::get(spec, "checkout"), "provenance checkout", 1000)?;
    let cwd = util::resolve_path(Path::new(&checkout));
    let root = git(&cwd, &["rev-parse", "--show-toplevel"])?;
    if !Path::new(&checkout).is_absolute() || root != cwd.display().to_string() {
        return Err(FlowError::progress(
            "provenance checkout must be an absolute repository root",
        ));
    }
    Ok(cwd)
}

fn provenance_repo(spec: &J) -> Result<(String, String)> {
    let repo = model::string(spec, "repo").unwrap_or_default();
    if !regex::Regex::new(r"^[\w.-]+/[\w.-]+$")
        .expect("static repo regex")
        .is_match(&repo)
    {
        return Err(FlowError::progress("explicit owner/repo required"));
    }
    let host = match model::get(spec, "host") {
        None => "github.com".to_string(),
        Some(value) => value
            .as_str()
            .map(ToString::to_string)
            .ok_or_else(|| FlowError::progress("invalid PR host"))?,
    };
    if !regex::Regex::new(r"^[A-Za-z0-9.-]+$")
        .expect("static host regex")
        .is_match(&host)
    {
        return Err(FlowError::progress("invalid PR host"));
    }
    Ok((repo, host))
}

fn provenance_origin(cwd: &Path, repo: &str, host: &str) -> Result<()> {
    let origin = git(cwd, &["remote", "get-url", "origin"])?;
    let accepted = [
        format!("https://{host}/{repo}.git"),
        format!("https://{host}/{repo}"),
        format!("git@{host}:{repo}.git"),
    ];
    if !accepted.contains(&origin) {
        return Err(FlowError::progress(
            "provenance checkout origin does not match repo/host",
        ));
    }
    Ok(())
}

pub fn provenance(spec: &J) -> Result<J> {
    let cwd = provenance_checkout(spec)?;
    provenance_with_checkout(spec, cwd)
}

fn provenance_with_checkout(spec: &J, cwd: PathBuf) -> Result<J> {
    let (repo, host) = provenance_repo(spec)?;
    provenance_with_origin(spec, cwd, RepositoryIdentity { repo, host })
}

fn provenance_with_origin(
    spec: &J,
    cwd: PathBuf,
    repository_identity: RepositoryIdentity,
) -> Result<J> {
    provenance_origin(&cwd, &repository_identity.repo, &repository_identity.host)?;
    provenance_base(spec, cwd, repository_identity)
}

fn provenance_base(spec: &J, cwd: PathBuf, repository_identity: RepositoryIdentity) -> Result<J> {
    let base = sha(model::get(spec, "original_base"))?;
    provenance_head(spec, cwd, repository_identity, base)
}

fn provenance_head(
    spec: &J,
    cwd: PathBuf,
    repository_identity: RepositoryIdentity,
    base: String,
) -> Result<J> {
    let head = sha(model::get(spec, "head"))?;
    provenance_ancestor(spec, cwd, repository_identity, base, head)
}

fn provenance_ancestor(
    spec: &J,
    cwd: PathBuf,
    repository_identity: RepositoryIdentity,
    base: String,
    head: String,
) -> Result<J> {
    git(&cwd, &["merge-base", "--is-ancestor", &base, &head])?;
    provenance_commits(spec, cwd, repository_identity, base, head)
}

fn provenance_commits(
    spec: &J,
    cwd: PathBuf,
    repository_identity: RepositoryIdentity,
    base: String,
    head: String,
) -> Result<J> {
    let own = git(&cwd, &["rev-list", "--reverse", &format!("{base}..{head}")])?;
    provenance_merges(spec, cwd, repository_identity, base, head, own)
}

fn provenance_merges(
    spec: &J,
    cwd: PathBuf,
    repository_identity: RepositoryIdentity,
    base: String,
    head: String,
    own: String,
) -> Result<J> {
    let merges = git(&cwd, &["rev-list", "--merges", &format!("{base}..{head}")])?;
    provenance_record(spec, cwd, repository_identity, base, head, own, merges)
}

fn provenance_record(
    spec: &J,
    cwd: PathBuf,
    repository_identity: RepositoryIdentity,
    base: String,
    head: String,
    own: String,
    merges: String,
) -> Result<J> {
    let own = provenance_lines(&own);
    let expected = provenance_expected(spec)?;
    if provenance_invalid(&own, expected.as_ref(), &merges) {
        return Err(FlowError::progress(
            "own_commits must exactly match the nonempty ordered linear original_base..head range (max 256)",
        ));
    }
    Ok(provenance_document(
        cwd,
        repository_identity,
        base,
        head,
        own,
    ))
}

fn provenance_lines(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToString::to_string)
        .collect()
}

fn provenance_expected(spec: &J) -> Result<Option<Vec<String>>> {
    let Some(items) = model::array_field(spec, "own_commits") else {
        return Ok(None);
    };
    items
        .iter()
        .map(|item| {
            item.as_str()
                .map(ToString::to_string)
                .ok_or_else(|| FlowError::progress("own_commits entries must be full SHAs"))
        })
        .collect::<Result<Vec<_>>>()
        .map(Some)
}

fn provenance_invalid(own: &[String], expected: Option<&Vec<String>>, merges: &str) -> bool {
    [
        own.is_empty(),
        own.len() > 256,
        expected.map(Vec::as_slice) != Some(own),
        !merges.trim().is_empty(),
    ]
    .into_iter()
    .any(|invalid| invalid)
}

fn provenance_document(
    cwd: PathBuf,
    repository_identity: RepositoryIdentity,
    original_base: String,
    head: String,
    own_commits: Vec<String>,
) -> J {
    json!({
        "checkout": cwd.display().to_string(),
        "repo": repository_identity.repo,
        "host": repository_identity.host,
        "original_base": original_base,
        "head": head,
        "own_commits": own_commits
    })
}

pub fn mutable(s: &J, spec: &J) -> Result<J> {
    let _ = progress::object(spec, "PR spec")?;
    let t = progress::task(
        s,
        model::get(spec, "task"),
        model::get(spec, "state_version"),
    )?;
    progress::require_unowned(&t, false)?;
    if model::bool_or(s, "stopped", false)
        || model::string(&t, "control").as_deref() != Some("running")
        || model::string(&t, "phase").as_deref() == Some("completed")
    {
        return Err(FlowError::progress(
            "PR task is stopped, paused, cancelled or completed",
        ));
    }
    Ok(t)
}

pub fn store(s: &mut J, mut t: J, now: f64, action: &str) -> Result<J> {
    let version = model::int(&t, "state_version").unwrap_or(0) + 1;
    t["state_version"] = json!(version);
    progress::audit(&mut t, now, action);
    let task_name = model::string(&t, "task").unwrap_or_default();
    s["tasks"][&task_name] = t.clone();
    Ok(t)
}

struct DependencyInfo {
    dep: J,
    dt: J,
    dr: J,
    landing: J,
    pinned_mismatch: bool,
}

struct DependencyScan {
    refreshed: Vec<J>,
    pending: bool,
    stop: Option<String>,
}

impl DependencyScan {
    fn new(capacity: usize) -> Self {
        Self {
            refreshed: Vec::with_capacity(capacity),
            pending: false,
            stop: None,
        }
    }

    fn stopped(mut self, reason: &str) -> Self {
        self.stop = Some(reason.to_string());
        self
    }

    fn pending(mut self, dep: J) -> Self {
        self.pending = true;
        self.refreshed.push(dep);
        self
    }

    fn landed(mut self, mut dep: J, landing: J) -> Self {
        dep["landing"] = landing;
        self.refreshed.push(dep);
        self
    }
}

pub fn dependency_state(
    s: &J,
    r: &mut J,
    api: Option<&dyn GitHubApi>,
    target: Option<&str>,
) -> Result<Option<String>> {
    let scan = dependency_scan(s, r, api, target)?;
    Ok(dependency_finish(r, scan))
}

fn dependency_scan(
    s: &J,
    r: &J,
    api: Option<&dyn GitHubApi>,
    target: Option<&str>,
) -> Result<DependencyScan> {
    let dependencies = model::array_field(r, "dependencies")
        .cloned()
        .unwrap_or_default();
    let initial = DependencyScan::new(dependencies.len());
    dependencies.into_iter().try_fold(initial, |scan, dep| {
        dependency_step(scan, s, r, api, target, dep)
    })
}

fn dependency_step(
    scan: DependencyScan,
    s: &J,
    r: &J,
    api: Option<&dyn GitHubApi>,
    target: Option<&str>,
    dep: J,
) -> Result<DependencyScan> {
    if scan.stop.is_some() {
        return Ok(scan);
    }
    dependency_step_live(scan, s, r, api, target, dep)
}

fn dependency_step_live(
    scan: DependencyScan,
    s: &J,
    r: &J,
    api: Option<&dyn GitHubApi>,
    target: Option<&str>,
    dep: J,
) -> Result<DependencyScan> {
    let info = dependency_info(s, dep);
    if dependency_changed(&info, r) {
        return Ok(scan.stopped("dependency_changed"));
    }
    dependency_step_pending_check(scan, r, api, target, info)
}

fn dependency_step_pending_check(
    scan: DependencyScan,
    r: &J,
    api: Option<&dyn GitHubApi>,
    target: Option<&str>,
    info: DependencyInfo,
) -> Result<DependencyScan> {
    if dependency_pending(&info) {
        return Ok(scan.pending(info.dep));
    }
    dependency_step_landing_check(scan, r, api, target, info)
}

fn dependency_step_landing_check(
    scan: DependencyScan,
    r: &J,
    api: Option<&dyn GitHubApi>,
    target: Option<&str>,
    info: DependencyInfo,
) -> Result<DependencyScan> {
    if dependency_landing_changed(&info, r) {
        return Ok(scan.stopped("dependency_changed"));
    }
    dependency_step_remote_check(scan, r, api, target, info)
}

fn dependency_step_remote_check(
    scan: DependencyScan,
    r: &J,
    api: Option<&dyn GitHubApi>,
    target: Option<&str>,
    info: DependencyInfo,
) -> Result<DependencyScan> {
    if let Some(api) = api {
        return dependency_step_compare(scan, r, api, target, info);
    }
    Ok(scan.landed(info.dep, info.landing))
}

fn dependency_step_compare(
    scan: DependencyScan,
    r: &J,
    api: &dyn GitHubApi,
    target: Option<&str>,
    info: DependencyInfo,
) -> Result<DependencyScan> {
    let commit = sha(model::get(&info.landing, "commit"))?;
    dependency_step_api_compare(scan, r, api, target, info, commit)
}

fn dependency_step_api_compare(
    scan: DependencyScan,
    r: &J,
    api: &dyn GitHubApi,
    target: Option<&str>,
    info: DependencyInfo,
    commit: String,
) -> Result<DependencyScan> {
    let target = dependency_target(r, target);
    let comparison = api.get(&format!("compare/{commit}...{target}"))?;
    Ok(dependency_step_comparison(scan, info, comparison, commit))
}

fn dependency_step_comparison(
    scan: DependencyScan,
    info: DependencyInfo,
    comparison: J,
    commit: String,
) -> DependencyScan {
    if dependency_not_on_target(&comparison, &commit) {
        return scan.stopped("dependency_not_on_target");
    }
    scan.landed(info.dep, info.landing)
}

fn dependency_info(s: &J, dep: J) -> DependencyInfo {
    let task_name = model::string(&dep, "task").unwrap_or_default();
    let dt = model::dig(s, &["tasks", &task_name])
        .cloned()
        .unwrap_or_else(|| json!({}));
    let dr = model::get(&dt, "pr").cloned().unwrap_or_else(|| json!({}));
    let landing = model::get(&dr, "landing").cloned().unwrap_or(Value::Null);
    let pinned = model::get(&dep, "landing").filter(|value| model::truthy(Some(value)));
    let pinned_mismatch = dependency_pinned_mismatch(&landing, pinned);
    DependencyInfo {
        dep,
        dt,
        dr,
        landing,
        pinned_mismatch,
    }
}

fn dependency_pinned_mismatch(landing: &J, pinned: Option<&J>) -> bool {
    pinned
        .map(|pinned| dependency_pinned_differs(landing, pinned))
        .unwrap_or(false)
}

fn dependency_pinned_differs(landing: &J, pinned: &J) -> bool {
    landing.is_null()
        || ["head", "commit", "number", "repo", "base_ref"]
            .iter()
            .any(|key| model::get(landing, key) != model::get(pinned, key))
}

fn dependency_changed(info: &DependencyInfo, r: &J) -> bool {
    [
        model::string(&info.dr, "head") != model::string(&info.dep, "head"),
        model::string(&info.dt, "candidate") != model::string(&info.dep, "head"),
        model::string(&info.dr, "repo") != model::string(r, "repo"),
        model::string(&info.dr, "host") != model::string(r, "host"),
        info.pinned_mismatch,
    ]
    .into_iter()
    .any(|changed| changed)
}

fn dependency_pending(info: &DependencyInfo) -> bool {
    [
        info.landing.is_null(),
        model::string(&info.dr, "phase").as_deref() != Some("merged"),
        model::string(&info.dt, "control").as_deref() == Some("cancelled"),
    ]
    .into_iter()
    .any(|pending| pending)
}

fn dependency_landing_changed(info: &DependencyInfo, r: &J) -> bool {
    [
        model::string(&info.landing, "head") != model::string(&info.dep, "head"),
        model::string(&info.landing, "base_ref") != model::string(r, "base_ref"),
        model::string(&info.landing, "repo") != model::string(r, "repo"),
        model::int(&info.landing, "number") != model::int(&info.dr, "number"),
    ]
    .into_iter()
    .any(|changed| changed)
}

fn dependency_target(r: &J, target: Option<&str>) -> String {
    target
        .map(ToString::to_string)
        .or_else(|| model::string(r, "target_base"))
        .unwrap_or_default()
}

fn dependency_not_on_target(comparison: &J, commit: &str) -> bool {
    !matches!(
        model::string(comparison, "status").as_deref(),
        Some("ahead" | "identical")
    ) || model::dig_str(comparison, &["merge_base_commit", "sha"]).as_deref() != Some(commit)
}

fn dependency_finish(r: &mut J, scan: DependencyScan) -> Option<String> {
    if let Some(stop) = scan.stop {
        return Some(stop);
    }
    r["dependencies"] = Value::Array(scan.refreshed);
    dependency_pending_status(scan.pending)
}

fn dependency_pending_status(pending: bool) -> Option<String> {
    if pending {
        return Some("dependency_pending".to_string());
    }
    None
}

pub fn prepare(s: &mut J, spec: &J, now: f64) -> Result<J> {
    let t = mutable(s, spec)?;
    prepare_new(s, spec, now, t)
}

fn prepare_new(s: &mut J, spec: &J, now: f64, t: J) -> Result<J> {
    if model::get(&t, "pr")
        .map(|value| model::truthy(Some(value)))
        .unwrap_or(false)
    {
        return Err(FlowError::progress(
            "PR intent already exists; reconcile it instead of creating another",
        ));
    }
    prepare_provenance(s, spec, now, t)
}

fn prepare_provenance(s: &mut J, spec: &J, now: f64, t: J) -> Result<J> {
    let record = provenance(spec)?;
    prepare_candidate(s, spec, now, t, record)
}

fn prepare_candidate(s: &mut J, spec: &J, now: f64, t: J, record: J) -> Result<J> {
    if model::string(&record, "head") != model::string(&t, "candidate") {
        return Err(FlowError::progress(
            "PR head must match the current task candidate",
        ));
    }
    prepare_refs(s, spec, now, t, record)
}

fn prepare_refs(s: &mut J, spec: &J, now: f64, t: J, record: J) -> Result<J> {
    let record = prepare_ref_keys(spec, record)?;
    prepare_issue(s, spec, now, t, record)
}

fn prepare_ref_keys(spec: &J, record: J) -> Result<J> {
    ["base_ref", "head_ref"]
        .into_iter()
        .try_fold(record, |record, key| prepare_ref(spec, record, key))
}

fn prepare_ref(spec: &J, record: J, key: &str) -> Result<J> {
    let reference = progress::text(model::get(spec, key), key, 250)?;
    prepare_ref_git(record, key, reference)
}

fn prepare_ref_git(mut record: J, key: &str, reference: String) -> Result<J> {
    let checkout = model::string(&record, "checkout").unwrap_or_default();
    git(
        Path::new(&checkout),
        &["check-ref-format", "--branch", &reference],
    )?;
    record[key] = json!(reference);
    Ok(record)
}

fn prepare_issue(s: &mut J, spec: &J, now: f64, t: J, record: J) -> Result<J> {
    let issue = model::int(spec, "issue").unwrap_or(0);
    if issue < 1 {
        return Err(FlowError::progress("PR issue must be a positive number"));
    }
    prepare_max_repairs(s, spec, now, t, record, issue)
}

fn prepare_max_repairs(s: &mut J, spec: &J, now: f64, t: J, record: J, issue: i64) -> Result<J> {
    let max_repairs = prepare_max_repairs_value(spec);
    if !(1..=10).contains(&max_repairs) {
        return Err(FlowError::progress("max_repairs must be 1..10"));
    }
    prepare_dependencies(
        s,
        spec,
        now,
        t,
        record,
        PreparationPolicy { issue, max_repairs },
    )
}

fn prepare_max_repairs_value(spec: &J) -> i64 {
    if model::get(spec, "max_repairs").is_none() {
        return 2;
    }
    prepare_max_repairs_number(spec)
}

fn prepare_max_repairs_number(spec: &J) -> i64 {
    match model::get(spec, "max_repairs") {
        Some(Value::Number(number)) => number.as_i64().unwrap_or(0),
        _ => 0,
    }
}

fn prepare_dependencies(
    s: &mut J,
    spec: &J,
    now: f64,
    t: J,
    record: J,
    preparation_policy: PreparationPolicy,
) -> Result<J> {
    let resolved = prepare_resolve_dependencies(s, spec, &record, &t)?;
    prepare_target_base(s, spec, now, t, record, resolved, preparation_policy)
}

fn prepare_resolve_dependencies(s: &J, spec: &J, record: &J, t: &J) -> Result<Vec<J>> {
    let dependencies = prepare_dependencies_list(spec)?;
    dependencies
        .into_iter()
        .try_fold(Vec::new(), |resolved, dep| {
            prepare_resolve_dependency(s, record, t, resolved, dep)
        })
}

fn prepare_dependencies_list(spec: &J) -> Result<Vec<J>> {
    let Some(dependencies) = model::get(spec, "dependencies").and_then(Value::as_array) else {
        return Err(FlowError::progress(
            "explicit dependencies list required (max 32)",
        ));
    };
    prepare_dependencies_count(dependencies)
}

fn prepare_dependencies_count(dependencies: &[J]) -> Result<Vec<J>> {
    if dependencies.len() > 32 {
        return Err(FlowError::progress(
            "explicit dependencies list required (max 32)",
        ));
    }
    Ok(dependencies.to_vec())
}

fn prepare_resolve_dependency(
    s: &J,
    record: &J,
    t: &J,
    resolved: Vec<J>,
    dep: J,
) -> Result<Vec<J>> {
    progress::object(&dep, "dependency")?;
    prepare_dependency_task(s, record, t, resolved, dep)
}

fn prepare_dependency_task(s: &J, record: &J, t: &J, resolved: Vec<J>, dep: J) -> Result<Vec<J>> {
    let Some(dt) =
        model::string(&dep, "task").and_then(|task| model::dig(s, &["tasks", &task]).cloned())
    else {
        return Err(FlowError::progress(
            "dependency must name a unique registered task and exact source head",
        ));
    };
    let Some(dr) = model::get(&dt, "pr")
        .filter(|value| model::truthy(Some(value)))
        .cloned()
    else {
        return Err(FlowError::progress(
            "dependency must name a unique registered task and exact source head",
        ));
    };
    prepare_dependency_head(record, t, resolved, dep, dt, dr)
}

fn prepare_dependency_head(
    record: &J,
    t: &J,
    resolved: Vec<J>,
    dep: J,
    dt: J,
    dr: J,
) -> Result<Vec<J>> {
    let dep_head = sha(model::get(&dep, "head"))?;
    prepare_dependency_unique(record, t, resolved, dt, dr, dep_head)
}

fn prepare_dependency_unique(
    record: &J,
    t: &J,
    resolved: Vec<J>,
    dt: J,
    dr: J,
    dep_head: String,
) -> Result<Vec<J>> {
    if prepare_dependency_conflict(record, t, &resolved, &dt, &dr, &dep_head) {
        return Err(FlowError::progress(
            "dependency must name a unique registered task and exact source head",
        ));
    }
    prepare_dependency_git(record, resolved, dt, dr, dep_head)
}

fn prepare_dependency_conflict(
    record: &J,
    t: &J,
    resolved: &[J],
    dt: &J,
    dr: &J,
    dep_head: &str,
) -> bool {
    [
        model::string(dr, "repo") != model::string(record, "repo"),
        model::string(dr, "host") != model::string(record, "host"),
        model::string(dr, "head").as_deref() != Some(dep_head),
        model::string(dt, "task") == model::string(t, "task"),
        resolved
            .iter()
            .any(|existing| model::string(existing, "task") == model::string(dt, "task")),
    ]
    .into_iter()
    .any(|conflict| conflict)
}

fn prepare_dependency_git(
    record: &J,
    mut resolved: Vec<J>,
    dt: J,
    dr: J,
    dep_head: String,
) -> Result<Vec<J>> {
    let checkout = model::string(record, "checkout").unwrap_or_default();
    let base = model::string(record, "original_base").unwrap_or_default();
    git(
        Path::new(&checkout),
        &["merge-base", "--is-ancestor", &dep_head, &base],
    )?;
    resolved.push(json!({
        "task": dt.get("task").cloned().unwrap_or(Value::Null),
        "head": dep_head,
        "landing": dr.get("landing").cloned().unwrap_or(Value::Null)
    }));
    Ok(resolved)
}

fn prepare_target_base(
    s: &mut J,
    spec: &J,
    now: f64,
    t: J,
    record: J,
    resolved: Vec<J>,
    preparation_policy: PreparationPolicy,
) -> Result<J> {
    let target_base = sha(model::get(spec, "target_base"))?;
    prepare_assemble(s, now, t, record, resolved, preparation_policy, target_base)
}

fn prepare_assemble(
    s: &mut J,
    now: f64,
    mut t: J,
    mut record: J,
    resolved: Vec<J>,
    preparation_policy: PreparationPolicy,
    target_base: String,
) -> Result<J> {
    record["issue"] = json!(preparation_policy.issue);
    record["dependencies"] = Value::Array(resolved);
    record["number"] = Value::Null;
    record["max_repairs"] = json!(preparation_policy.max_repairs);
    record["repairs"] = json!({});
    record["versions"] = json!([]);
    record["phase"] = json!("publication_prepared");
    record["target_base"] = json!(target_base);
    record["landing"] = Value::Null;
    record["observation"] = Value::Null;
    record["publication"] = json!({
        "kind": "create",
        "phase": "prepared",
        "prepared_at": now,
        "head": record.get("head").cloned().unwrap_or(Value::Null)
    });
    progress::ownership::publication(s, &t, &record)?;
    t["pr"] = record;
    store(s, t, now, "pr_prepared")
}

pub fn publication_begin(s: &mut J, spec: &J, now: f64) -> Result<J> {
    let t = mutable(s, spec)?;
    publication_record(s, spec, now, t)
}

fn publication_record(s: &mut J, spec: &J, now: f64, t: J) -> Result<J> {
    let r = model::get(&t, "pr")
        .cloned()
        .ok_or_else(|| FlowError::progress("PR provenance is unavailable"))?;
    publication_authorization(s, spec, now, PullRequestState { task: t, record: r })
}

fn publication_authorization(
    s: &mut J,
    spec: &J,
    now: f64,
    pull_request_state: PullRequestState,
) -> Result<J> {
    let authorization = model::string(spec, "authorization").unwrap_or_default();
    if !publication_authorized(&pull_request_state.task, &authorization) {
        return Err(FlowError::progress(
            "publication requires an existing explicit publication authorization",
        ));
    }
    publication_phase(s, now, pull_request_state, authorization)
}

fn publication_authorized(t: &J, authorization: &str) -> bool {
    model::dig(t, &["authorization", authorization])
        .map(|auth| model::string(auth, "action").as_deref() == Some("publication"))
        .unwrap_or(false)
}

fn publication_phase(
    s: &mut J,
    now: f64,
    pull_request_state: PullRequestState,
    authorization: String,
) -> Result<J> {
    let op_phase = model::dig_str(&pull_request_state.record, &["publication", "phase"]);
    if op_phase.as_deref() != Some("prepared") {
        return Err(FlowError::progress(
            "publication may already have executed; reconcile, never replay",
        ));
    }
    publication_check_inputs(s, now, pull_request_state, authorization)
}

fn publication_check_inputs(
    s: &mut J,
    now: f64,
    pull_request_state: PullRequestState,
    authorization: String,
) -> Result<J> {
    progress::ownership::publication(s, &pull_request_state.task, &pull_request_state.record)?;
    progress::ownership::publication_slot(s, &pull_request_state.task, &pull_request_state.record)?;
    let api = GitHub::new(pull_request_state.record.clone());
    publication_check(s, &pull_request_state.task, now, Some(&api))?;
    publication_dependencies(s, now, pull_request_state, authorization, &api)
}

fn publication_dependencies(
    s: &mut J,
    now: f64,
    mut pull_request_state: PullRequestState,
    authorization: String,
    api: &dyn GitHubApi,
) -> Result<J> {
    publication_require_dependencies(s, &mut pull_request_state.record, api)?;
    publication_kind(s, now, pull_request_state, authorization, api)
}

fn publication_require_dependencies(s: &J, r: &mut J, api: &dyn GitHubApi) -> Result<()> {
    let dependency = dependency_state(s, r, Some(api), None)?;
    publication_no_dependency(dependency)
}

fn publication_no_dependency(dependency: Option<String>) -> Result<()> {
    if dependency.is_some() {
        return Err(FlowError::progress(
            "dependency changed, pending or absent from validated target; inspect before publication",
        ));
    }
    Ok(())
}

fn publication_kind(
    s: &mut J,
    now: f64,
    pull_request_state: PullRequestState,
    authorization: String,
    api: &dyn GitHubApi,
) -> Result<J> {
    let kind = model::dig_str(&pull_request_state.record, &["publication", "kind"])
        .unwrap_or_else(|| "create".to_string());
    publication_update(s, now, pull_request_state, authorization, api, kind)
}

fn publication_update(
    s: &mut J,
    now: f64,
    pull_request_state: PullRequestState,
    authorization: String,
    api: &dyn GitHubApi,
    kind: String,
) -> Result<J> {
    if kind == "update" {
        return publication_update_checked(s, now, pull_request_state, authorization, api);
    }
    publication_commit(s, now, pull_request_state.task, authorization)
}

fn publication_update_checked(
    s: &mut J,
    now: f64,
    pull_request_state: PullRequestState,
    authorization: String,
    api: &dyn GitHubApi,
) -> Result<J> {
    publication_check_update(&pull_request_state.record, api)?;
    publication_commit(s, now, pull_request_state.task, authorization)
}

fn publication_check_update(r: &J, api: &dyn GitHubApi) -> Result<()> {
    let number = model::int(r, "number").unwrap_or(0);
    let pr = api.get(&format!("pulls/{number}"))?;
    let previous_head = model::dig_str(r, &["publication", "previous_head"]).unwrap_or_default();
    publication_update_guard(r, &pr, &previous_head)
}

fn publication_update_guard(r: &J, pr: &J, previous_head: &str) -> Result<()> {
    if publication_update_changed(r, pr, previous_head) {
        return Err(FlowError::progress(
            "same-PR update inputs changed; inspect instead of publishing",
        ));
    }
    Ok(())
}

fn publication_update_changed(r: &J, pr: &J, previous_head: &str) -> bool {
    [
        !pr_identity(r, pr),
        model::dig_str(pr, &["head", "sha"]).as_deref() != Some(previous_head),
        model::dig_str(pr, &["base", "sha"]) != model::string(r, "target_base"),
        model::string(pr, "state").as_deref() != Some("open"),
        model::bool_or(pr, "merged", false),
    ]
    .into_iter()
    .any(|changed| changed)
}

fn publication_commit(s: &mut J, now: f64, mut t: J, authorization: String) -> Result<J> {
    t["pr"]["publication"]["phase"] = json!("sending");
    t["pr"]["publication"]["started_at"] = json!(now);
    t["pr"]["publication"]["authorization"] = json!(authorization);
    t["pr"]["phase"] = json!("publication_uncertain");
    store(s, t, now, "pr_publication_started")
}

pub fn pr_identity(r: &J, pr: &J) -> bool {
    model::dig_str(pr, &["head", "repo", "full_name"]) == model::string(r, "repo")
        && model::dig_str(pr, &["base", "repo", "full_name"]) == model::string(r, "repo")
        && model::dig_str(pr, &["head", "ref"]) == model::string(r, "head_ref")
        && model::dig_str(pr, &["base", "ref"]) == model::string(r, "base_ref")
}

fn recover_pr(api: &dyn GitHubApi, r: &J) -> Result<Option<J>> {
    if let Some(number) = model::int(r, "number").filter(|number| *number > 0) {
        return recover_registered(api, r, number);
    }
    recover_search(api, r)
}

fn recover_registered(api: &dyn GitHubApi, r: &J, number: i64) -> Result<Option<J>> {
    let pr = api.get(&format!("pulls/{number}"))?;
    recover_registered_guard(r, pr, number)
}

fn recover_registered_guard(r: &J, pr: J, number: i64) -> Result<Option<J>> {
    if recover_registered_changed(r, &pr, number) {
        return Err(FlowError::progress("registered PR identity changed"));
    }
    Ok(Some(pr))
}

fn recover_registered_changed(r: &J, pr: &J, number: i64) -> bool {
    model::int(pr, "number") != Some(number) || !pr_identity(r, pr)
}

struct RecoverScan {
    matches: Vec<J>,
    complete: bool,
}

fn recover_search(api: &dyn GitHubApi, r: &J) -> Result<Option<J>> {
    let query = recover_query(r);
    let scan = recover_pages(api, r, &query)?;
    recover_select(api, r, scan)
}

fn recover_query(r: &J) -> String {
    let repo = model::string(r, "repo").unwrap_or_default();
    let owner = repo.split('/').next().unwrap_or_default();
    let head_ref = model::string(r, "head_ref").unwrap_or_default();
    let base_ref = model::string(r, "base_ref").unwrap_or_default();
    urlencode_query(&[
        ("state", "all"),
        ("head", &format!("{owner}:{head_ref}")),
        ("base", &base_ref),
    ])
}

fn recover_pages(api: &dyn GitHubApi, r: &J, query: &str) -> Result<RecoverScan> {
    let initial = RecoverScan {
        matches: Vec::new(),
        complete: false,
    };
    (1..=100).try_fold(initial, |scan, page| {
        recover_page(api, r, query, scan, page)
    })
}

fn recover_page(
    api: &dyn GitHubApi,
    r: &J,
    query: &str,
    scan: RecoverScan,
    page: i32,
) -> Result<RecoverScan> {
    if scan.complete {
        return Ok(scan);
    }
    recover_page_fetch(api, r, query, scan, page)
}

fn recover_page_fetch(
    api: &dyn GitHubApi,
    r: &J,
    query: &str,
    scan: RecoverScan,
    page: i32,
) -> Result<RecoverScan> {
    let chunk = api.get(&format!("pulls?{query}&per_page=100&page={page}"))?;
    recover_page_apply(r, scan, chunk)
}

fn recover_page_apply(r: &J, mut scan: RecoverScan, chunk: J) -> Result<RecoverScan> {
    let Some(chunk) = chunk.as_array() else {
        return Err(FlowError::progress("PR lookup response is not a list"));
    };
    scan.matches
        .extend(chunk.iter().filter(|pr| pr_identity(r, pr)).cloned());
    scan.complete = chunk.len() < 100;
    Ok(scan)
}

fn recover_select(api: &dyn GitHubApi, r: &J, scan: RecoverScan) -> Result<Option<J>> {
    if !scan.complete {
        return Err(FlowError::progress("PR recovery pagination incomplete"));
    }
    recover_select_unique(api, r, scan)
}

fn recover_select_unique(api: &dyn GitHubApi, r: &J, scan: RecoverScan) -> Result<Option<J>> {
    if scan.matches.len() > 1 {
        return Err(FlowError::progress(
            "multiple PRs match publication intent; inspect without creating another",
        ));
    }
    recover_select_first(api, r, scan.matches)
}

fn recover_select_first(api: &dyn GitHubApi, r: &J, matches: Vec<J>) -> Result<Option<J>> {
    let Some(first) = matches.first() else {
        return Ok(None);
    };
    let number = model::int(first, "number").unwrap_or(0);
    recover_select_verify(api, r, number)
}

fn recover_select_verify(api: &dyn GitHubApi, r: &J, number: i64) -> Result<Option<J>> {
    let pr = api.get(&format!("pulls/{number}"))?;
    recover_select_guard(r, pr, number)
}

fn recover_select_guard(r: &J, pr: J, number: i64) -> Result<Option<J>> {
    if recover_select_changed(r, &pr, number) {
        return Err(FlowError::progress("PR changed during recovery lookup"));
    }
    Ok(Some(pr))
}

fn recover_select_changed(r: &J, pr: &J, number: i64) -> bool {
    model::int(pr, "number") != Some(number) || !pr_identity(r, pr)
}

fn confirm_landing(api: &dyn GitHubApi, r: &mut J, pr: &J, actual: &str, now: f64) -> Result<()> {
    let merge = sha(model::get(pr, "merge_commit_sha"))?;
    let base_ref = model::string(r, "base_ref").unwrap_or_default();
    let latest = sha(model::dig(
        &api.get(&format!(
            "branches/{}",
            crate::util::pyjson::quote_component(&base_ref)
        ))?,
        &["commit", "sha"],
    ))?;
    let comparison = api.get(&format!("compare/{merge}...{latest}"))?;
    let status = model::string(&comparison, "status").unwrap_or_default();
    if !matches!(status.as_str(), "ahead" | "identical")
        || model::dig_str(&comparison, &["merge_base_commit", "sha"]).as_deref()
            != Some(merge.as_str())
    {
        return Err(FlowError::progress(
            "merged PR landing is not confirmed on current target branch",
        ));
    }
    r["landing"] = json!({
        "head": actual,
        "commit": merge,
        "latest_base": latest,
        "observed_at": now,
        "number": pr.get("number").cloned().unwrap_or(Value::Null),
        "repo": r.get("repo").cloned().unwrap_or(Value::Null),
        "base_ref": r.get("base_ref").cloned().unwrap_or(Value::Null)
    });
    r["phase"] = json!("merged");
    Ok(())
}

fn classify_pr(
    s: &J,
    t: &J,
    r: &mut J,
    pr: &J,
    actual: &str,
    now: f64,
    api: &dyn GitHubApi,
) -> Result<()> {
    if model::string(r, "head").as_deref() != Some(actual) {
        classify_head(r, actual);
        return Ok(());
    }
    classify_owner(s, t, r, pr, actual, now, api)
}

fn classify_head(r: &mut J, actual: &str) {
    let phase = classify_head_phase(r, actual);
    r["phase"] = json!(phase);
}

fn classify_head_phase(r: &J, actual: &str) -> &'static str {
    if model::dig_str(r, &["publication", "kind"]).as_deref() != Some("update") {
        return "head_changed";
    }
    classify_head_updated(r, actual)
}

fn classify_head_updated(r: &J, actual: &str) -> &'static str {
    if model::dig_str(r, &["publication", "previous_head"]).as_deref() == Some(actual) {
        return "update_unconfirmed";
    }
    "head_changed"
}

fn classify_owner(
    s: &J,
    t: &J,
    r: &mut J,
    pr: &J,
    actual: &str,
    now: f64,
    api: &dyn GitHubApi,
) -> Result<()> {
    if classify_other_owns(s, t, r, pr) {
        return Err(FlowError::progress("PR is already owned by another task"));
    }
    classify_registered(r, pr, actual, now, api)
}

fn classify_other_owns(s: &J, t: &J, r: &J, pr: &J) -> bool {
    model::get(s, "tasks")
        .and_then(Value::as_object)
        .map(|tasks| {
            tasks
                .values()
                .any(|other| classify_task_owns(other, t, r, pr))
        })
        .unwrap_or(false)
}

fn classify_task_owns(other: &J, t: &J, r: &J, pr: &J) -> bool {
    model::string(other, "task") != model::string(t, "task")
        && model::get(other, "pr")
            .filter(|value| model::truthy(Some(value)))
            .map(|other_pr| classify_same_pr(other_pr, r, pr))
            .unwrap_or(false)
}

fn classify_same_pr(other_pr: &J, r: &J, pr: &J) -> bool {
    (
        model::string(other_pr, "host"),
        model::string(other_pr, "repo"),
        model::int(other_pr, "number"),
    ) == (
        model::string(r, "host"),
        model::string(r, "repo"),
        model::int(pr, "number"),
    )
}

fn classify_registered(
    r: &mut J,
    pr: &J,
    actual: &str,
    now: f64,
    api: &dyn GitHubApi,
) -> Result<()> {
    classify_record_registered(r, pr, now);
    classify_status(r, pr, actual, now, api)
}

fn classify_record_registered(r: &mut J, pr: &J, now: f64) {
    r["number"] = pr.get("number").cloned().unwrap_or(Value::Null);
    r["publication"]["phase"] = json!("observed");
    r["publication"]["observed_at"] = json!(now);
    r["phase"] = json!("registered");
}

fn classify_status(r: &mut J, pr: &J, actual: &str, now: f64, api: &dyn GitHubApi) -> Result<()> {
    if model::bool_or(pr, "merged", false) {
        return confirm_landing(api, r, pr, actual, now);
    }
    classify_not_merged(r, pr)
}

fn classify_not_merged(r: &mut J, pr: &J) -> Result<()> {
    if model::string(pr, "state").as_deref() != Some("open") {
        r["phase"] = json!("closed_unmerged");
        return Ok(());
    }
    classify_base(r, pr)
}

fn classify_base(r: &mut J, pr: &J) -> Result<()> {
    if model::dig_str(pr, &["base", "sha"]) != model::string(r, "target_base") {
        r["phase"] = json!("base_changed");
        return Ok(());
    }
    classify_mergeable(r, pr)
}

fn classify_mergeable(r: &mut J, pr: &J) -> Result<()> {
    if model::get(pr, "mergeable") == Some(&json!(false)) {
        r["phase"] = json!("conflict");
        return Ok(());
    }
    classify_auto_merge(r, pr)
}

fn classify_auto_merge(r: &mut J, pr: &J) -> Result<()> {
    if !model::truthy(model::get(pr, "auto_merge")) {
        r["phase"] = json!("native_merge_not_enabled");
    }
    Ok(())
}

pub fn reconcile(s: &mut J, spec: &J, now: f64, api: Option<&dyn GitHubApi>) -> Result<J> {
    let t = mutable(s, spec)?;
    reconcile_started(s, now, api, t)
}

fn reconcile_started(s: &mut J, now: f64, api: Option<&dyn GitHubApi>, t: J) -> Result<J> {
    let r = model::get(&t, "pr")
        .cloned()
        .ok_or_else(|| FlowError::progress("PR provenance is unavailable"))?;
    reconcile_api(s, now, api, PullRequestState { task: t, record: r })
}

fn reconcile_api(
    s: &mut J,
    now: f64,
    api: Option<&dyn GitHubApi>,
    pull_request_state: PullRequestState,
) -> Result<J> {
    if let Some(api) = api {
        return reconcile_lookup(s, now, api, pull_request_state);
    }
    let owned = GitHub::new(pull_request_state.record.clone());
    reconcile_lookup(s, now, &owned, pull_request_state)
}

fn reconcile_lookup(
    s: &mut J,
    now: f64,
    api: &dyn GitHubApi,
    pull_request_state: PullRequestState,
) -> Result<J> {
    let Some(pr) = recover_pr(api, &pull_request_state.record)? else {
        return reconcile_not_found(s, now, pull_request_state);
    };
    reconcile_found(s, now, api, pull_request_state, pr)
}

fn reconcile_not_found(s: &mut J, now: f64, mut pull_request_state: PullRequestState) -> Result<J> {
    pull_request_state.record["phase"] = json!("publication_unconfirmed");
    pull_request_state.task["pr"] = pull_request_state.record;
    store(s, pull_request_state.task, now, "pr_not_found")
}

fn reconcile_found(
    s: &mut J,
    now: f64,
    api: &dyn GitHubApi,
    pull_request_state: PullRequestState,
    pr: J,
) -> Result<J> {
    let actual = sha(model::get(&pr, "head").and_then(|head| head.get("sha")))?;
    reconcile_found_remote(s, now, api, pull_request_state, pr, actual)
}

fn reconcile_found_remote(
    s: &mut J,
    now: f64,
    api: &dyn GitHubApi,
    pull_request_state: PullRequestState,
    pr: J,
    actual: String,
) -> Result<J> {
    let remote = reconcile_remote(&pr, &actual, now)?;
    reconcile_dependency(s, now, api, pull_request_state, pr, actual, remote)
}

fn reconcile_remote(pr: &J, actual: &str, now: f64) -> Result<J> {
    let base = sha(model::get(pr, "base").and_then(|base| base.get("sha")))?;
    Ok(json!({
        "head": actual,
        "base": base,
        "state": pr.get("state").cloned().unwrap_or(Value::Null),
        "merged": model::bool_or(pr, "merged", false),
        "auto_merge": model::truthy(model::get(pr, "auto_merge")),
        "mergeable": pr.get("mergeable").cloned().unwrap_or(Value::Null),
        "mergeable_state": pr.get("mergeable_state").cloned().unwrap_or(Value::Null),
        "at": now
    }))
}

fn reconcile_dependency(
    s: &mut J,
    now: f64,
    api: &dyn GitHubApi,
    mut pull_request_state: PullRequestState,
    pr: J,
    actual: String,
    remote: J,
) -> Result<J> {
    pull_request_state.record["remote"] = remote;
    pull_request_state.record["observation"] = Value::Null;
    let remote_base = model::dig_str(&pull_request_state.record, &["remote", "base"]);
    let dependency = dependency_state(
        s,
        &mut pull_request_state.record,
        Some(api),
        remote_base.as_deref(),
    )?;
    reconcile_decision(s, now, api, pull_request_state, pr, actual, dependency)
}

fn reconcile_decision(
    s: &mut J,
    now: f64,
    api: &dyn GitHubApi,
    mut pull_request_state: PullRequestState,
    pr: J,
    actual: String,
    dependency: Option<String>,
) -> Result<J> {
    if let Some(dependency) = dependency {
        pull_request_state.record["phase"] = json!(dependency);
        pull_request_state.task["pr"] = pull_request_state.record;
        return store(s, pull_request_state.task, now, "pr_dependency_decision");
    }
    reconcile_classify(s, now, api, pull_request_state, pr, actual)
}

fn reconcile_classify(
    s: &mut J,
    now: f64,
    api: &dyn GitHubApi,
    mut pull_request_state: PullRequestState,
    pr: J,
    actual: String,
) -> Result<J> {
    classify_pr(
        s,
        &pull_request_state.task,
        &mut pull_request_state.record,
        &pr,
        &actual,
        now,
        api,
    )?;
    pull_request_state.task["pr"] = pull_request_state.record;
    store(s, pull_request_state.task, now, "pr_reconciled")
}

type ObserverData = (PathBuf, J, J, String);

type ObserverIdentity = (
    Option<String>,
    Option<Value>,
    Option<String>,
    Option<String>,
    Option<Value>,
    Option<String>,
);

fn load_observer(spec: &J, t: &J, r: &J, now: f64) -> Result<ObserverData> {
    let source = progress::text(model::get(spec, "source"), "CI observer directory", 1000)?;
    load_observer_source(t, r, now, source)
}

fn load_observer_source(t: &J, r: &J, now: f64, source: String) -> Result<ObserverData> {
    if !Path::new(&source).is_absolute() {
        return Err(FlowError::progress("observer directory must be absolute"));
    }
    load_observer_identity(t, r, now, util::resolve_path(Path::new(&source)))
}

fn load_observer_identity(t: &J, r: &J, now: f64, source: PathBuf) -> Result<ObserverData> {
    let identity = util::read_json(&source.join("identity.json"))?;
    load_observer_status(t, r, now, source, identity)
}

fn load_observer_status(
    t: &J,
    r: &J,
    now: f64,
    source: PathBuf,
    identity: J,
) -> Result<ObserverData> {
    let result = util::read_json(&source.join("status.json"))?;
    load_observer_config(t, r, now, source, identity, result)
}

fn load_observer_config(
    t: &J,
    r: &J,
    now: f64,
    source: PathBuf,
    identity: J,
    result: J,
) -> Result<ObserverData> {
    let config = load_observer_validate(&identity)?;
    load_observer_matches(t, r, now, source, identity, result, config)
}

fn load_observer_validate(identity: &J) -> Result<J> {
    let Some(config) = model::get(identity, "config") else {
        return Err(FlowError::progress("CI observer identity is incomplete"));
    };
    crate::ci::validate(config)
}

fn load_observer_matches(
    t: &J,
    r: &J,
    now: f64,
    source: PathBuf,
    identity: J,
    result: J,
    config: J,
) -> Result<ObserverData> {
    let digest = sha256_json_sorted(&config);
    if load_observer_mismatch(t, r, &identity, &result, &config, &digest) {
        return Err(FlowError::progress(
            "CI observer does not match exact task/epoch/repo/PR/head",
        ));
    }
    load_observer_fresh(now, source, result, config, digest)
}

fn load_observer_mismatch(
    t: &J,
    r: &J,
    identity: &J,
    result: &J,
    config: &J,
    digest: &str,
) -> bool {
    let identity_hash = model::string(identity, "config_hash").unwrap_or_default();
    let result_hash = model::string(result, "config_hash").unwrap_or_default();
    [
        digest != identity_hash.as_str(),
        digest != result_hash.as_str(),
        load_observer_expected(config) != load_observer_actual(t, r),
    ]
    .into_iter()
    .any(|mismatch| mismatch)
}

fn load_observer_expected(config: &J) -> ObserverIdentity {
    (
        model::string(config, "task"),
        model::get(config, "epoch").cloned(),
        model::string(config, "repo"),
        Some(model::string_or(config, "host", "github.com")),
        model::get(config, "pr").cloned(),
        model::string(config, "sha"),
    )
}

fn load_observer_actual(t: &J, r: &J) -> ObserverIdentity {
    (
        model::string(t, "task"),
        model::get(t, "epoch").cloned(),
        model::string(r, "repo"),
        model::string(r, "host"),
        model::get(r, "number").cloned(),
        model::string(r, "head"),
    )
}

fn load_observer_fresh(
    now: f64,
    source: PathBuf,
    result: J,
    config: J,
    digest: String,
) -> Result<ObserverData> {
    let observed_at = model::number(&result, "observed_at").unwrap_or(0.0);
    if !(0.0..=120.0).contains(&(now - observed_at)) {
        return Err(FlowError::progress(
            "CI observation is stale or from the future",
        ));
    }
    Ok((source, result, config, digest))
}

struct WorkflowScan {
    failures: Vec<J>,
    runs_seen: Vec<J>,
}

impl WorkflowScan {
    fn new() -> Self {
        Self {
            failures: Vec::new(),
            runs_seen: Vec::new(),
        }
    }
}

fn inspect_workflows(api: &dyn GitHubApi, r: &J, config: &J, result: &J) -> Result<WorkflowScan> {
    let workflows = model::array_field(config, "workflows")
        .cloned()
        .unwrap_or_default();
    let scan = workflows
        .into_iter()
        .try_fold(WorkflowScan::new(), |scan, workflow| {
            inspect_workflow(api, r, result, scan, workflow)
        })?;
    Ok(scan)
}

fn inspect_workflow(
    api: &dyn GitHubApi,
    r: &J,
    result: &J,
    scan: WorkflowScan,
    workflow: J,
) -> Result<WorkflowScan> {
    let file = model::string(&workflow, "file").unwrap_or_default();
    let event = model::string(&workflow, "event").unwrap_or_default();
    let rows = inspect_workflow_rows(
        result,
        &WorkflowIdentity {
            file: &file,
            event: &event,
        },
    );
    if !inspect_workflow_exact(&rows, &workflow) {
        return Err(FlowError::progress(
            "repair observation requires exact required_jobs inventory",
        ));
    }
    inspect_workflow_row(
        api,
        r,
        scan,
        &workflow,
        &WorkflowIdentity {
            file: &file,
            event: &event,
        },
        &rows,
    )
}

fn inspect_workflow_rows(result: &J, workflow_identity: &WorkflowIdentity<'_>) -> Vec<J> {
    model::array_field(result, "workflows")
        .map(|rows| {
            rows.iter()
                .filter(|row| inspect_workflow_row_matches(row, workflow_identity))
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

fn inspect_workflow_row_matches(row: &J, workflow_identity: &WorkflowIdentity<'_>) -> bool {
    model::string(row, "file").as_deref() == Some(workflow_identity.file)
        && model::string(row, "event").as_deref() == Some(workflow_identity.event)
}

fn inspect_workflow_exact(rows: &[J], workflow: &J) -> bool {
    rows.len() == 1 && model::string(workflow, "decision").as_deref() == Some("required_jobs")
}

fn inspect_workflow_row(
    api: &dyn GitHubApi,
    r: &J,
    scan: WorkflowScan,
    workflow: &J,
    workflow_identity: &WorkflowIdentity<'_>,
    rows: &[J],
) -> Result<WorkflowScan> {
    let row = &rows[0];
    let scan = inspect_workflow_seen(scan, row, workflow_identity);
    inspect_workflow_run(api, r, scan, workflow, row, workflow_identity)
}

fn inspect_workflow_seen(
    mut scan: WorkflowScan,
    row: &J,
    workflow_identity: &WorkflowIdentity<'_>,
) -> WorkflowScan {
    scan.runs_seen.push(json!({
        "workflow": workflow_identity.file,
        "event": workflow_identity.event,
        "run": row.get("run_id").cloned().unwrap_or(Value::Null),
        "attempt": row.get("attempt").cloned().unwrap_or(Value::Null)
    }));
    scan
}

fn inspect_workflow_run(
    api: &dyn GitHubApi,
    r: &J,
    scan: WorkflowScan,
    workflow: &J,
    row: &J,
    workflow_identity: &WorkflowIdentity<'_>,
) -> Result<WorkflowScan> {
    if let Some(run_id) = model::int(row, "run_id") {
        return inspect_workflow_verify_run(api, r, scan, workflow, row, workflow_identity, run_id);
    }
    inspect_workflow_jobs(scan, workflow, row, workflow_identity)
}

fn inspect_workflow_verify_run(
    api: &dyn GitHubApi,
    r: &J,
    scan: WorkflowScan,
    workflow: &J,
    row: &J,
    workflow_identity: &WorkflowIdentity<'_>,
    run_id: i64,
) -> Result<WorkflowScan> {
    let head = model::string(r, "head").unwrap_or_default();
    let query = urlencode_query(&[("head_sha", &head), ("event", workflow_identity.event)]);
    let runs = api.pages(
        &format!("actions/workflows/{}/runs?{query}", workflow_identity.file),
        "workflow_runs",
    )?;
    inspect_workflow_verify_attempt(scan, workflow, row, workflow_identity, run_id, &runs, &head)
}

fn inspect_workflow_verify_attempt(
    scan: WorkflowScan,
    workflow: &J,
    row: &J,
    workflow_identity: &WorkflowIdentity<'_>,
    run_id: i64,
    runs: &[J],
    head: &str,
) -> Result<WorkflowScan> {
    if inspect_workflow_attempt_changed(row, workflow_identity.event, runs, run_id, head) {
        return Err(FlowError::progress(
            "CI run/attempt changed; refresh the existing observer",
        ));
    }
    inspect_workflow_jobs(scan, workflow, row, workflow_identity)
}

fn inspect_workflow_attempt_changed(
    row: &J,
    event: &str,
    runs: &[J],
    run_id: i64,
    head: &str,
) -> bool {
    let matching: Vec<&J> = runs
        .iter()
        .filter(|run| inspect_workflow_run_matches(run, head, event))
        .collect();
    let attempt = model::int(row, "attempt").unwrap_or(0);
    let Some(current) = matching
        .iter()
        .max_by_key(|run| model::int(run, "id").unwrap_or(0))
    else {
        return true;
    };
    !inspect_workflow_attempt_matches(current, run_id, attempt)
}

fn inspect_workflow_run_matches(run: &J, head: &str, event: &str) -> bool {
    model::string(run, "head_sha").as_deref() == Some(head)
        && model::string(run, "event").as_deref() == Some(event)
}

fn inspect_workflow_attempt_matches(current: &J, run_id: i64, attempt: i64) -> bool {
    model::int(current, "id") == Some(run_id) && model::int(current, "run_attempt") == Some(attempt)
}

fn inspect_workflow_jobs(
    scan: WorkflowScan,
    workflow: &J,
    row: &J,
    workflow_identity: &WorkflowIdentity<'_>,
) -> Result<WorkflowScan> {
    let required = inspect_workflow_required_jobs(workflow);
    let jobs = model::array_field(row, "jobs").cloned().unwrap_or_default();
    let mut scan = scan;
    for job in jobs {
        scan = inspect_workflow_job(scan, &required, &job, row, workflow_identity);
    }
    Ok(scan)
}

fn inspect_workflow_required_jobs(workflow: &J) -> std::collections::BTreeSet<String> {
    model::array_field(workflow, "jobs")
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(ToString::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn inspect_workflow_job(
    mut scan: WorkflowScan,
    required: &std::collections::BTreeSet<String>,
    job: &J,
    row: &J,
    workflow_identity: &WorkflowIdentity<'_>,
) -> WorkflowScan {
    let name = model::string(job, "name").unwrap_or_default();
    if inspect_workflow_job_failed(required, job, &name) {
        scan.failures.push(json!({
            "workflow": workflow_identity.file,
            "event": workflow_identity.event,
            "run": row.get("run_id").cloned().unwrap_or(Value::Null),
            "attempt": row.get("attempt").cloned().unwrap_or(Value::Null),
            "job": job.get("id").cloned().unwrap_or(Value::Null),
            "name": name,
            "source": "github-actions"
        }));
    }
    scan
}

fn inspect_workflow_job_failed(
    required: &std::collections::BTreeSet<String>,
    job: &J,
    name: &str,
) -> bool {
    [
        required.contains(name),
        model::string(job, "status").as_deref() == Some("completed"),
        model::string(job, "conclusion").as_deref() == Some("failure"),
    ]
    .into_iter()
    .all(|failed| failed)
}

fn inspect_checks(api: &dyn GitHubApi, r: &J, config: &J, result: &J) -> Result<(Vec<J>, Vec<J>)> {
    if inspect_checks_empty(config) {
        return Ok((Vec::new(), Vec::new()));
    }
    inspect_checks_live(api, r, config, result)
}

fn inspect_checks_empty(config: &J) -> bool {
    model::array_field(config, "checks")
        .map(Vec::is_empty)
        .unwrap_or(true)
}

fn inspect_checks_live(
    api: &dyn GitHubApi,
    r: &J,
    config: &J,
    result: &J,
) -> Result<(Vec<J>, Vec<J>)> {
    let current = check_policy::observe(config, api)?;
    inspect_checks_verified(r, config, result, current)
}

fn inspect_checks_verified(
    r: &J,
    config: &J,
    result: &J,
    current: Vec<J>,
) -> Result<(Vec<J>, Vec<J>)> {
    inspect_checks_require(r, config, result, &current)?;
    Ok(inspect_check_rows(&current))
}

fn inspect_checks_require(r: &J, config: &J, result: &J, current: &[J]) -> Result<()> {
    if inspect_checks_changed(r, config, result, current) {
        return Err(FlowError::progress(
            "external Check or its validated base changed; refresh the existing observer",
        ));
    }
    Ok(())
}

fn inspect_checks_changed(r: &J, config: &J, result: &J, current: &[J]) -> bool {
    [
        model::get(result, "checks")
            .cloned()
            .unwrap_or_else(|| json!([]))
            != Value::Array(current.to_vec()),
        model::string(config, "target_base") != model::string(r, "target_base"),
    ]
    .into_iter()
    .any(|changed| changed)
}

fn inspect_check_rows(current: &[J]) -> (Vec<J>, Vec<J>) {
    let mut failures = Vec::new();
    let mut runs_seen = Vec::new();
    for row in current {
        inspect_check_row(row, &mut failures, &mut runs_seen);
    }
    (failures, runs_seen)
}

fn inspect_check_row(row: &J, failures: &mut Vec<J>, runs_seen: &mut Vec<J>) {
    if let Some(check_run) = model::get(row, "check_run") {
        runs_seen.push(json!({
            "workflow": row.get("name").cloned().unwrap_or(Value::Null),
            "event": "external_check",
            "run": check_run,
            "attempt": 1
        }));
    }
    inspect_check_failure(row, failures);
}

fn inspect_check_failure(row: &J, failures: &mut Vec<J>) {
    if model::string(row, "state").as_deref() == Some("failure") {
        failures.push(json!({
            "workflow": row.get("name").cloned().unwrap_or(Value::Null),
            "event": "external_check",
            "run": row.get("check_run").cloned().unwrap_or(Value::Null),
            "attempt": 1,
            "job": row.get("check_run").cloned().unwrap_or(Value::Null),
            "name": row.get("name").cloned().unwrap_or(Value::Null),
            "source": "github-checks",
            "app_id": row.get("app_id").cloned().unwrap_or(Value::Null),
            "external_id": row.get("external_id").cloned().unwrap_or(Value::Null)
        }));
    }
}

fn observed_phase(state: &str, failures: &[J]) -> &'static str {
    if observed_repair_needed(state, failures) {
        return "repair_needed";
    }
    observed_phase_after(state)
}

fn observed_repair_needed(state: &str, failures: &[J]) -> bool {
    state == "failure" && !failures.is_empty()
}

fn observed_phase_after(state: &str) -> &'static str {
    if state == "success" {
        return "checks_satisfied";
    }
    observed_phase_attention(state)
}

fn observed_phase_attention(state: &str) -> &'static str {
    if observed_attention_state(state) {
        return "needs_attention";
    }
    "checks_pending"
}

fn observed_attention_state(state: &str) -> bool {
    [
        "failure",
        "needs_attention",
        "timeout",
        "api_error",
        "superseded",
    ]
    .into_iter()
    .any(|candidate| candidate == state)
}

pub fn observe(s: &mut J, spec: &J, now: f64, api: Option<&dyn GitHubApi>) -> Result<J> {
    let mut working = s.clone();
    let t = reconcile(&mut working, spec, now, api)?;
    observe_task(s, spec, now, api, working, t)
}

fn observe_task(
    s: &mut J,
    spec: &J,
    now: f64,
    api: Option<&dyn GitHubApi>,
    working: J,
    t: J,
) -> Result<J> {
    let r = model::get(&t, "pr")
        .cloned()
        .ok_or_else(|| FlowError::progress("PR provenance is unavailable"))?;
    observe_api(
        s,
        spec,
        now,
        api,
        working,
        PullRequestState { task: t, record: r },
    )
}

fn observe_api(
    s: &mut J,
    spec: &J,
    now: f64,
    api: Option<&dyn GitHubApi>,
    working: J,
    pull_request_state: PullRequestState,
) -> Result<J> {
    if let Some(api) = api {
        return observe_eligible(s, spec, now, api, working, pull_request_state);
    }
    let owned = GitHub::new(pull_request_state.record.clone());
    observe_eligible(s, spec, now, &owned, working, pull_request_state)
}

fn observe_eligible(
    s: &mut J,
    spec: &J,
    now: f64,
    api: &dyn GitHubApi,
    working: J,
    pull_request_state: PullRequestState,
) -> Result<J> {
    if !observe_phase_eligible(&pull_request_state.record) {
        return observe_early(s, pull_request_state.task);
    }
    observe_collect(s, spec, now, api, working, pull_request_state)
}

fn observe_phase_eligible(r: &J) -> bool {
    matches!(
        model::string(r, "phase").as_deref(),
        Some("registered" | "native_merge_not_enabled" | "conflict")
    )
}

fn observe_early(s: &mut J, t: J) -> Result<J> {
    s["tasks"][&model::string(&t, "task").unwrap_or_default()] = t.clone();
    Ok(t)
}

fn observe_collect(
    s: &mut J,
    spec: &J,
    now: f64,
    api: &dyn GitHubApi,
    working: J,
    pull_request_state: PullRequestState,
) -> Result<J> {
    let (source, result, config, digest) = load_observer(
        spec,
        &pull_request_state.task,
        &pull_request_state.record,
        now,
    )?;
    observe_workflows(
        s,
        now,
        api,
        ObservationState {
            working,
            source,
            result,
            digest,
        },
        pull_request_state,
        config,
    )
}

fn observe_workflows(
    s: &mut J,
    now: f64,
    api: &dyn GitHubApi,
    observation_state: ObservationState,
    pull_request_state: PullRequestState,
    config: J,
) -> Result<J> {
    let scan = inspect_workflows(
        api,
        &pull_request_state.record,
        &config,
        &observation_state.result,
    )?;
    observe_checks(
        s,
        now,
        api,
        observation_state,
        pull_request_state,
        config,
        scan,
    )
}

fn observe_checks(
    s: &mut J,
    now: f64,
    api: &dyn GitHubApi,
    observation_state: ObservationState,
    pull_request_state: PullRequestState,
    config: J,
    mut scan: WorkflowScan,
) -> Result<J> {
    let (check_failures, check_runs) = inspect_checks(
        api,
        &pull_request_state.record,
        &config,
        &observation_state.result,
    )?;
    scan.failures.extend(check_failures);
    scan.runs_seen.extend(check_runs);
    Ok(observe_record(
        s,
        observation_state,
        pull_request_state,
        now,
        scan,
    ))
}

fn observe_record(
    s: &mut J,
    mut observation_state: ObservationState,
    mut pull_request_state: PullRequestState,
    now: f64,
    mut scan: WorkflowScan,
) -> J {
    observe_sort(&mut scan.failures, &mut scan.runs_seen);
    let token = observe_token(
        &pull_request_state.record,
        &scan.runs_seen,
        &observation_state.digest,
    );
    pull_request_state.record["observation"] = json!({
        "token": token,
        "at": now,
        "state": observation_state.result.get("state").cloned().unwrap_or(Value::Null),
        "failures": scan.failures,
        "source": observation_state.source.join("status.json").display().to_string(),
        "config_hash": observation_state.digest
    });
    observe_set_phase(&mut pull_request_state.record, &observation_state.result);
    pull_request_state.task["pr"] = pull_request_state.record;
    observation_state.working["tasks"]
        [&model::string(&pull_request_state.task, "task").unwrap_or_default()] =
        pull_request_state.task.clone();
    s["tasks"] = observation_state.working["tasks"].clone();
    pull_request_state.task
}

fn observe_sort(failures: &mut [J], runs_seen: &mut [J]) {
    failures.sort_by(|left, right| {
        let key = |value: &J| {
            (
                model::string(value, "workflow").unwrap_or_default(),
                model::string(value, "event").unwrap_or_default(),
                model::get(value, "run")
                    .map(ToString::to_string)
                    .unwrap_or_default(),
                model::get(value, "attempt")
                    .map(ToString::to_string)
                    .unwrap_or_default(),
                model::get(value, "job")
                    .map(ToString::to_string)
                    .unwrap_or_default(),
            )
        };
        key(left).cmp(&key(right))
    });
    runs_seen.sort_by_key(|run| {
        (
            model::string(run, "workflow").unwrap_or_default(),
            model::string(run, "event").unwrap_or_default(),
        )
    });
}

fn observe_token(r: &J, runs_seen: &[J], digest: &str) -> String {
    sha256_json_sorted(&json!({
        "head": r.get("head").cloned().unwrap_or(Value::Null),
        "base": r.get("target_base").cloned().unwrap_or(Value::Null),
        "runs": runs_seen,
        "config_hash": digest
    }))
}

fn observe_set_phase(r: &mut J, result: &J) {
    if model::string(r, "phase").as_deref() != Some("registered") {
        return;
    }
    observe_assign_phase(r, result);
}

fn observe_assign_phase(r: &mut J, result: &J) {
    let state = model::string(result, "state").unwrap_or_default();
    let failures = model::get(r, "observation")
        .and_then(|observation| observation.get("failures"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    r["phase"] = json!(observed_phase(&state, &failures));
}

pub fn repair_intent(t: &mut J, action: &J, event: &J, now: f64) -> Result<()> {
    let Some(r) = model::get(t, "pr")
        .filter(|value| model::truthy(Some(value)))
        .cloned()
    else {
        return repair_intent_absent(action);
    };
    repair_intent_request(t, r, action, event, now)
}

fn repair_intent_absent(action: &J) -> Result<()> {
    if model::get(action, "pr_repair").is_some() {
        return Err(FlowError::progress("repair has no registered PR"));
    }
    Ok(())
}

fn repair_intent_request(t: &mut J, r: J, action: &J, event: &J, now: f64) -> Result<()> {
    let writes = repair_writes(action)?;
    repair_intent_writes(t, r, action, event, now, writes)
}

fn repair_writes(action: &J) -> Result<bool> {
    if model::get(action, "writes").is_none() {
        return Ok(true);
    }
    repair_writes_value(action)
}

fn repair_writes_value(action: &J) -> Result<bool> {
    match model::get(action, "writes") {
        Some(Value::Bool(value)) => Ok(*value),
        _ => Err(FlowError::progress("writes must be a boolean")),
    }
}

fn repair_intent_writes(
    t: &mut J,
    r: J,
    action: &J,
    event: &J,
    now: f64,
    writes: bool,
) -> Result<()> {
    if !writes {
        return repair_intent_read_only(action);
    }
    repair_intent_binding(t, r, action, event, now)
}

fn repair_intent_read_only(action: &J) -> Result<()> {
    if model::get(action, "pr_repair").is_some() {
        return Err(FlowError::progress(
            "a repair must reserve its isolated writer",
        ));
    }
    Ok(())
}

fn repair_intent_binding(t: &mut J, r: J, action: &J, event: &J, now: f64) -> Result<()> {
    let binding = progress::object_owned(
        model::get(action, "pr_repair"),
        "writing PR dispatch requires pr_repair binding",
    )?;
    repair_intent_observation(t, r, action, event, now, binding)
}

fn repair_intent_observation(
    t: &mut J,
    r: J,
    action: &J,
    event: &J,
    now: f64,
    binding: J,
) -> Result<()> {
    let observation = repair_observation_value(
        &r,
        "repair requires current exact PR/head/base/required failure and matching received CI event",
    )?;
    repair_intent_evidence(t, r, action, event, now, binding, observation)
}

fn repair_observation_value(r: &J, message: &str) -> Result<J> {
    model::get(r, "observation")
        .cloned()
        .filter(|value| !value.is_null())
        .ok_or_else(|| FlowError::progress(message))
}

fn repair_intent_evidence(
    t: &mut J,
    r: J,
    action: &J,
    event: &J,
    now: f64,
    binding: J,
    observation: J,
) -> Result<()> {
    let token = model::string(&observation, "token").unwrap_or_default();
    let expected = repair_expected_binding(&r, &token);
    let evidence = model::string(event, "evidence").unwrap_or_default();
    let evidence_path = util::resolve_path(Path::new(&evidence));
    if repair_intent_mismatch(
        &r,
        &binding,
        &observation,
        event,
        now,
        &RepairEvidence {
            expected: &expected,
            evidence: &evidence,
            evidence_path: &evidence_path,
        },
    ) {
        return Err(FlowError::progress(
            "repair requires current exact PR/head/base/required failure and matching received CI event",
        ));
    }
    repair_intent_attempts(t, r, action, now, observation, token)
}

fn repair_expected_binding(r: &J, token: &str) -> J {
    json!({
        "number": r.get("number").cloned().unwrap_or(Value::Null),
        "head": r.get("head").cloned().unwrap_or(Value::Null),
        "base": r.get("target_base").cloned().unwrap_or(Value::Null),
        "token": token
    })
}

fn repair_intent_mismatch(
    r: &J,
    binding: &J,
    observation: &J,
    event: &J,
    now: f64,
    repair_evidence: &RepairEvidence<'_>,
) -> bool {
    [
        model::string(r, "phase").as_deref() != Some("repair_needed"),
        binding != repair_evidence.expected,
        !(0.0..=120.0).contains(&(now - model::number(observation, "at").unwrap_or(0.0))),
        model::string(event, "outcome").as_deref() != Some("failed"),
        repair_evidence.evidence.is_empty(),
        repair_evidence.evidence_path.display().to_string()
            != model::string(observation, "source").unwrap_or_default(),
    ]
    .into_iter()
    .any(|mismatch| mismatch)
}

fn repair_intent_attempts(
    t: &mut J,
    r: J,
    action: &J,
    now: f64,
    observation: J,
    token: String,
) -> Result<()> {
    let max_repairs = model::int(&r, "max_repairs").unwrap_or(0);
    let repairs = repair_map(&r);
    if repair_attempt_exhausted(&repairs, &token, max_repairs) {
        return Err(FlowError::progress(
            "repair already dispatched or attempt limit reached; inspect instead of retrying",
        ));
    }
    repair_intent_scope(t, r, action, now, observation, token, repairs)
}

fn repair_map(r: &J) -> Map<String, Value> {
    model::get(r, "repairs")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default()
}

fn repair_attempt_exhausted(repairs: &Map<String, Value>, token: &str, max_repairs: i64) -> bool {
    [
        repairs.contains_key(token),
        repairs.len() >= max_repairs as usize,
    ]
    .into_iter()
    .any(|exhausted| exhausted)
}

fn repair_intent_scope(
    t: &mut J,
    r: J,
    action: &J,
    now: f64,
    observation: J,
    token: String,
    repairs: Map<String, Value>,
) -> Result<()> {
    let consumed = repair_consumed(&repairs);
    let failures = model::array_field(&observation, "failures")
        .cloned()
        .unwrap_or_default();
    if repair_scope_consumed(&failures, &consumed) {
        return Err(FlowError::progress(
            "repair already dispatched for this run/attempt; inspect instead of retrying",
        ));
    }
    repair_intent_diagnosis(t, r, action, now, token, failures)
}

fn repair_consumed(
    repairs: &Map<String, Value>,
) -> std::collections::BTreeSet<(String, String, String, String)> {
    repairs.values().flat_map(repair_used_scopes).collect()
}

fn repair_used_scopes(used: &J) -> Vec<(String, String, String, String)> {
    model::array_field(used, "failures")
        .map(|failures| failures.iter().map(repair_scope).collect())
        .unwrap_or_default()
}

fn repair_scope_consumed(
    failures: &[J],
    consumed: &std::collections::BTreeSet<(String, String, String, String)>,
) -> bool {
    failures
        .iter()
        .any(|failure| consumed.contains(&repair_scope(failure)))
}

fn repair_intent_diagnosis(
    t: &mut J,
    r: J,
    action: &J,
    now: f64,
    token: String,
    failures: Vec<J>,
) -> Result<()> {
    let diagnosis =
        progress::object_owned(model::get(action, "repair_diagnosis"), "repair diagnosis")?;
    repair_intent_kind(t, r, action, now, token, failures, diagnosis)
}

fn repair_intent_kind(
    t: &mut J,
    r: J,
    action: &J,
    now: f64,
    token: String,
    failures: Vec<J>,
    diagnosis: J,
) -> Result<()> {
    if model::string(&diagnosis, "kind").as_deref() != Some("code") {
        return Err(FlowError::progress(
            "only an inspected code defect may dispatch code repair",
        ));
    }
    repair_intent_diagnosis_evidence(t, r, action, now, token, failures, diagnosis)
}

fn repair_intent_diagnosis_evidence(
    t: &mut J,
    r: J,
    action: &J,
    now: f64,
    token: String,
    failures: Vec<J>,
    diagnosis: J,
) -> Result<()> {
    progress::text(
        model::get(&diagnosis, "evidence"),
        "code defect diagnosis evidence",
        1000,
    )?;
    repair_intent_workspace(t, r, action, now, token, failures)
}

fn repair_intent_workspace(
    t: &mut J,
    r: J,
    action: &J,
    now: f64,
    token: String,
    failures: Vec<J>,
) -> Result<()> {
    let workspace = progress::text(
        model::get(action, "workspace"),
        "isolated repair workspace",
        1000,
    )?;
    repair_intent_check_workspace(t, r, action, now, token, failures, workspace)
}

fn repair_intent_check_workspace(
    t: &mut J,
    r: J,
    action: &J,
    now: f64,
    token: String,
    failures: Vec<J>,
    workspace: String,
) -> Result<()> {
    let workspace = util::resolve_path(Path::new(&workspace));
    let original = PathBuf::from(model::string(&r, "checkout").unwrap_or_default());
    if util::overlap(&workspace, &original) {
        return Err(FlowError::progress(
            "repair must use an isolated checkout, preserving continued development",
        ));
    }
    repair_intent_store(t, r, action, now, token, failures)
}

fn repair_intent_store(
    t: &mut J,
    mut r: J,
    action: &J,
    now: f64,
    token: String,
    failures: Vec<J>,
) -> Result<()> {
    r["repairs"][&token] = json!({
        "operation": action.get("operation").cloned().unwrap_or(Value::Null),
        "failures": failures,
        "head": r.get("head").cloned().unwrap_or(Value::Null),
        "base": r.get("target_base").cloned().unwrap_or(Value::Null),
        "at": now
    });
    t["pr"] = r;
    Ok(())
}

fn repair_scope(failure: &J) -> (String, String, String, String) {
    (
        model::string(failure, "workflow").unwrap_or_default(),
        model::string(failure, "event").unwrap_or_default(),
        model::get(failure, "run")
            .map(ToString::to_string)
            .unwrap_or_default(),
        model::get(failure, "attempt")
            .map(ToString::to_string)
            .unwrap_or_default(),
    )
}

fn repair_binding(t: &J, op: &J) -> Result<Option<J>> {
    let intent = repair_binding_intent(op);
    if repair_binding_conflict(&intent) {
        return Err(FlowError::progress(
            "a repair must reserve its isolated writer",
        ));
    }
    let binding = model::get(&intent, "pr_repair").cloned();
    repair_binding_required(t, &intent, binding)
}

fn repair_binding_intent(op: &J) -> J {
    model::get(op, "intent")
        .cloned()
        .unwrap_or_else(|| json!({}))
}

fn repair_binding_conflict(intent: &J) -> bool {
    !model::bool_or(intent, "writes", true) && model::get(intent, "pr_repair").is_some()
}

fn repair_binding_required(t: &J, intent: &J, binding: Option<J>) -> Result<Option<J>> {
    if repair_binding_missing(t, intent, &binding) {
        return Err(FlowError::progress(
            "writing PR dispatch requires pr_repair binding",
        ));
    }
    Ok(binding)
}

fn repair_binding_missing(t: &J, intent: &J, binding: &Option<J>) -> bool {
    [
        binding.is_none(),
        model::get(t, "pr")
            .map(|value| model::truthy(Some(value)))
            .unwrap_or(false),
        model::bool_or(intent, "writes", true),
    ]
    .into_iter()
    .all(|missing| missing)
}

fn repair_observation(r: &J, binding: &J, now: f64) -> Result<J> {
    let observation = repair_observation_value(
        r,
        "repair observation expired or changed; reconcile without replaying",
    )?;
    repair_observation_guard(r, binding, now, observation)
}

fn repair_observation_guard(r: &J, binding: &J, now: f64, observation: J) -> Result<J> {
    if repair_observation_stale(r, binding, &observation, now) {
        return Err(FlowError::progress(
            "repair observation expired or changed; reconcile without replaying",
        ));
    }
    Ok(observation)
}

fn repair_observation_stale(r: &J, binding: &J, observation: &J, now: f64) -> bool {
    [
        model::string(r, "phase").as_deref() != Some("repair_needed"),
        model::string(binding, "token") != model::string(observation, "token"),
        model::string(binding, "head") != model::string(r, "head"),
        model::string(binding, "base") != model::string(r, "target_base"),
        !(0.0..=120.0).contains(&(now - model::number(observation, "at").unwrap_or(0.0))),
    ]
    .into_iter()
    .any(|stale| stale)
}

fn verify_check_failure(api: &dyn GitHubApi, r: &J, failure: &J) -> Result<()> {
    let config = verify_check_config(r, failure);
    let current = check_policy::observe(&config, api)?;
    verify_check_current(&current, failure)
}

fn verify_check_config(r: &J, failure: &J) -> J {
    json!({
        "repo": r.get("repo").cloned().unwrap_or(Value::Null),
        "pr": r.get("number").cloned().unwrap_or(Value::Null),
        "sha": r.get("head").cloned().unwrap_or(Value::Null),
        "target_base": r.get("target_base").cloned().unwrap_or(Value::Null),
        "base_ref": r.get("base_ref").cloned().unwrap_or(Value::Null),
        "checks": [{
            "name": failure.get("name").cloned().unwrap_or(Value::Null),
            "app_id": failure.get("app_id").cloned().unwrap_or(Value::Null),
            "external_id": failure.get("external_id").cloned().unwrap_or(Value::Null)
        }]
    })
}

fn verify_check_current(current: &[J], failure: &J) -> Result<()> {
    if verify_check_changed(current, failure) {
        return Err(FlowError::progress(
            "external Check changed before repair dispatch",
        ));
    }
    Ok(())
}

fn verify_check_changed(current: &[J], failure: &J) -> bool {
    let row = current.first();
    [
        current.len() != 1,
        row.and_then(|row| model::string(row, "state")).as_deref() != Some("failure"),
        row.and_then(|row| model::get(row, "check_run")) != model::get(failure, "run"),
    ]
    .into_iter()
    .any(|changed| changed)
}

fn verify_ci_failure(api: &dyn GitHubApi, r: &J, failure: &J) -> Result<()> {
    let workflow = model::string(failure, "workflow").unwrap_or_default();
    let event = model::string(failure, "event").unwrap_or_default();
    let head = model::string(r, "head").unwrap_or_default();
    let query = urlencode_query(&[("head_sha", &head), ("event", &event)]);
    let runs = api.pages(
        &format!("actions/workflows/{workflow}/runs?{query}"),
        "workflow_runs",
    )?;
    verify_ci_runs(&runs, &head, &event, failure)
}

fn verify_ci_runs(runs: &[J], head: &str, event: &str, failure: &J) -> Result<()> {
    if verify_ci_changed(runs, head, event, failure) {
        return Err(FlowError::progress(
            "CI attempt changed before repair dispatch",
        ));
    }
    Ok(())
}

fn verify_ci_changed(runs: &[J], head: &str, event: &str, failure: &J) -> bool {
    let matching: Vec<&J> = runs
        .iter()
        .filter(|run| inspect_workflow_run_matches(run, head, event))
        .collect();
    let Some(latest) = matching
        .iter()
        .max_by_key(|run| model::int(run, "id").unwrap_or(0))
    else {
        return true;
    };
    !verify_ci_attempt(latest, failure)
}

fn verify_ci_attempt(latest: &J, failure: &J) -> bool {
    model::int(latest, "id") == model::int(failure, "run")
        && model::int(latest, "run_attempt") == model::int(failure, "attempt")
}

fn verify_live_repair(api: &dyn GitHubApi, r: &J, o: &J) -> Result<()> {
    verify_live_pr_fetch(api, r)?;
    verify_live_failures(api, r, o)
}

fn verify_live_pr_fetch(api: &dyn GitHubApi, r: &J) -> Result<()> {
    let number = model::int(r, "number").unwrap_or(0);
    let pr = api.get(&format!("pulls/{number}"))?;
    verify_live_pr(r, &pr)
}

fn verify_live_pr(r: &J, pr: &J) -> Result<()> {
    if verify_live_pr_changed(r, pr) {
        return Err(FlowError::progress("PR changed before repair dispatch"));
    }
    Ok(())
}

fn verify_live_pr_changed(r: &J, pr: &J) -> bool {
    [
        !pr_identity(r, pr),
        model::dig_str(pr, &["head", "sha"]) != model::string(r, "head"),
        model::dig_str(pr, &["base", "sha"]) != model::string(r, "target_base"),
        model::string(pr, "state").as_deref() != Some("open"),
        model::bool_or(pr, "merged", false),
    ]
    .into_iter()
    .any(|changed| changed)
}

fn verify_live_failures(api: &dyn GitHubApi, r: &J, o: &J) -> Result<()> {
    let failures = model::array_field(o, "failures")
        .cloned()
        .unwrap_or_default();
    failures
        .iter()
        .try_for_each(|failure| verify_live_failure(api, r, failure))
}

fn verify_live_failure(api: &dyn GitHubApi, r: &J, failure: &J) -> Result<()> {
    if model::string(failure, "source").as_deref() == Some("github-checks") {
        return verify_check_failure(api, r, failure);
    }
    verify_ci_failure(api, r, failure)
}

pub fn check_repair(s: &J, t: &J, op: &J, now: f64, live: bool) -> Result<()> {
    let Some(binding) = repair_binding(t, op)? else {
        return Ok(());
    };
    let Some(r) = model::get(t, "pr")
        .filter(|value| model::truthy(Some(value)))
        .cloned()
    else {
        return Err(FlowError::progress("repair has no registered PR"));
    };
    check_repair_r(s, now, live, binding, r)
}

fn check_repair_r(s: &J, now: f64, live: bool, binding: J, r: J) -> Result<()> {
    if live {
        let api = GitHub::new(r.clone());
        return check_repair_dependency(s, Some(&api), r, now, binding);
    }
    check_repair_dependency(s, None, r, now, binding)
}

fn check_repair_dependency(
    s: &J,
    api: Option<&dyn GitHubApi>,
    r: J,
    now: f64,
    binding: J,
) -> Result<()> {
    check_repair_require_dependency(s, api, &r)?;
    check_repair_observation(r, now, binding, api)
}

fn check_repair_require_dependency(s: &J, api: Option<&dyn GitHubApi>, r: &J) -> Result<()> {
    let dependency = dependency_state(s, &mut r.clone(), api, None)?;
    check_repair_no_dependency(dependency)
}

fn check_repair_no_dependency(dependency: Option<String>) -> Result<()> {
    if dependency.is_some() {
        return Err(FlowError::progress(
            "dependency changed or pending before repair",
        ));
    }
    Ok(())
}

fn check_repair_observation(r: J, now: f64, binding: J, api: Option<&dyn GitHubApi>) -> Result<()> {
    let o = repair_observation(&r, &binding, now)?;
    check_repair_verify(api, &r, &o)
}

fn check_repair_verify(api: Option<&dyn GitHubApi>, r: &J, o: &J) -> Result<()> {
    if let Some(api) = api {
        return verify_live_repair(api, r, o);
    }
    Ok(())
}

pub fn update(s: &mut J, spec: &J, now: f64) -> Result<J> {
    let t = mutable(s, spec)?;
    update_record(s, spec, now, t)
}

fn update_record(s: &mut J, spec: &J, now: f64, t: J) -> Result<J> {
    let r = model::get(&t, "pr")
        .cloned()
        .ok_or_else(|| FlowError::progress("PR provenance is unavailable"))?;
    update_check(s, spec, now, PullRequestState { task: t, record: r })
}

fn update_check(s: &mut J, spec: &J, now: f64, pull_request_state: PullRequestState) -> Result<J> {
    let operation = model::string(spec, "operation").unwrap_or_default();
    let event_id = model::string(spec, "event").unwrap_or_default();
    let op = model::dig(&pull_request_state.task, &["operations", &operation]).cloned();
    let event = model::dig(s, &["events", &event_id]).cloned();
    if update_invalid(
        &pull_request_state.task,
        &pull_request_state.record,
        &op,
        &event,
    ) {
        return Err(FlowError::progress(
            "repair update requires applied exact accepted repair result and inspected writer release",
        ));
    }
    update_used(s, spec, now, pull_request_state, operation, op)
}

fn update_invalid(t: &J, r: &J, op: &Option<J>, event: &Option<J>) -> bool {
    let writer = model::get(t, "writer").map(Value::is_null).unwrap_or(true);
    let blocked = model::string(t, "phase").as_deref() == Some("blocked");
    let accepted = op
        .as_ref()
        .map(|op| model::string(op, "phase").as_deref() == Some("accepted"))
        .unwrap_or(false);
    let repair = op
        .as_ref()
        .and_then(|op| model::get(op, "intent"))
        .and_then(|intent| model::get(intent, "pr_repair"))
        .is_some();
    let assignment = op
        .as_ref()
        .map(|op| model::string(op, "assignment") == model::string(t, "assignment"))
        .unwrap_or(false);
    let applied = event
        .as_ref()
        .map(|event| model::bool_or(event, "applied", false))
        .unwrap_or(false);
    let complete = event
        .as_ref()
        .map(|event| model::string(event, "outcome").as_deref() == Some("complete"))
        .unwrap_or(false);
    let binding = event
        .as_ref()
        .map(|event| {
            model::get(event, "binding")
                .map(|binding| progress::same(t, binding))
                .unwrap_or(false)
        })
        .unwrap_or(false);
    let observed = model::dig_str(r, &["publication", "phase"]).as_deref() == Some("observed");
    [
        !writer,
        !blocked,
        !accepted,
        !repair,
        !assignment,
        !applied,
        !complete,
        !binding,
        !observed,
    ]
    .into_iter()
    .any(|invalid| invalid)
}

fn update_used(
    s: &mut J,
    spec: &J,
    now: f64,
    pull_request_state: PullRequestState,
    operation: String,
    op: Option<J>,
) -> Result<J> {
    let op = op.unwrap_or(Value::Null);
    let token = model::dig_str(&op, &["intent", "pr_repair", "token"]).unwrap_or_default();
    let used = model::dig(&pull_request_state.record, &["repairs", &token]).cloned();
    if !update_used_ok(used.as_ref(), &operation, &pull_request_state.record) {
        return Err(FlowError::progress(
            "repair update has no matching consumed failure intent",
        ));
    }
    update_candidate(s, spec, now, pull_request_state, operation, op)
}

fn update_used_ok(used: Option<&J>, operation: &str, r: &J) -> bool {
    let Some(used) = used else {
        return false;
    };
    [
        model::string(used, "operation").as_deref() == Some(operation),
        model::string(used, "head") == model::string(r, "head"),
    ]
    .into_iter()
    .all(|matches| matches)
}

fn update_candidate(
    s: &mut J,
    spec: &J,
    now: f64,
    pull_request_state: PullRequestState,
    operation: String,
    op: J,
) -> Result<J> {
    let candidate = update_provenance(spec, &pull_request_state.record)?;
    update_candidate_check(s, spec, now, pull_request_state, operation, op, candidate)
}

fn update_provenance(spec: &J, r: &J) -> Result<J> {
    provenance(&update_candidate_spec(spec, r))
}

fn update_candidate_spec(spec: &J, r: &J) -> J {
    let checkout = model::string(spec, "checkout").unwrap_or_default();
    json!({
        "checkout": checkout,
        "repo": r.get("repo").cloned().unwrap_or(Value::Null),
        "host": r.get("host").cloned().unwrap_or(Value::Null),
        "original_base": r.get("original_base").cloned().unwrap_or(Value::Null),
        "head": spec.get("head").cloned().unwrap_or(Value::Null),
        "own_commits": spec.get("own_commits").cloned().unwrap_or(Value::Null)
    })
}

fn update_candidate_check(
    s: &mut J,
    spec: &J,
    now: f64,
    pull_request_state: PullRequestState,
    operation: String,
    op: J,
    candidate: J,
) -> Result<J> {
    if update_append_invalid(&pull_request_state.record, &candidate) {
        return Err(FlowError::progress(
            "repair must append to the registered own commits, not replace inherited changes",
        ));
    }
    update_workspace_check(s, spec, now, pull_request_state, operation, op, candidate)
}

fn update_append_invalid(r: &J, candidate: &J) -> bool {
    let own_commits = model::array_field(r, "own_commits")
        .cloned()
        .unwrap_or_default();
    let candidate_own = model::array_field(candidate, "own_commits")
        .cloned()
        .unwrap_or_default();
    if candidate_own.len() < own_commits.len() {
        return true;
    }
    [
        model::string(candidate, "head") == model::string(r, "head"),
        candidate_own[..own_commits.len()] != own_commits[..],
    ]
    .into_iter()
    .any(|invalid| invalid)
}

fn update_workspace_check(
    s: &mut J,
    spec: &J,
    now: f64,
    pull_request_state: PullRequestState,
    operation: String,
    op: J,
    candidate: J,
) -> Result<J> {
    if model::string(&candidate, "checkout") != model::dig_str(&op, &["intent", "workspace"]) {
        return Err(FlowError::progress(
            "repair candidate must come from the accepted isolated workspace",
        ));
    }
    update_evidence(s, spec, now, pull_request_state, operation, candidate)
}

fn update_evidence(
    s: &mut J,
    spec: &J,
    now: f64,
    pull_request_state: PullRequestState,
    operation: String,
    candidate: J,
) -> Result<J> {
    let evidence = progress::text(
        model::get(spec, "evidence"),
        "repair verification evidence",
        1000,
    )?;
    update_versions(s, now, pull_request_state, operation, candidate, evidence)
}

fn update_versions(
    s: &mut J,
    now: f64,
    mut pull_request_state: PullRequestState,
    operation: String,
    candidate: J,
    evidence: String,
) -> Result<J> {
    let entry = update_version_entry(&pull_request_state.record);
    if let Some(versions) =
        model::get_mut(&mut pull_request_state.record, "versions").and_then(Value::as_array_mut)
    {
        versions.push(entry);
    }
    update_assign(s, now, pull_request_state, operation, candidate, evidence)
}

fn update_version_entry(r: &J) -> J {
    json!({
        "head": r.get("head").cloned().unwrap_or(Value::Null),
        "own_commits": r.get("own_commits").cloned().unwrap_or(Value::Null),
        "observation": r.get("observation").cloned().unwrap_or(Value::Null)
    })
}

fn update_assign(
    s: &mut J,
    now: f64,
    mut pull_request_state: PullRequestState,
    operation: String,
    candidate: J,
    evidence: String,
) -> Result<J> {
    let previous = model::string(&pull_request_state.record, "head").unwrap_or_default();
    pull_request_state.record["head"] = candidate.get("head").cloned().unwrap_or(Value::Null);
    pull_request_state.record["own_commits"] =
        candidate.get("own_commits").cloned().unwrap_or(Value::Null);
    pull_request_state.record["observation"] = Value::Null;
    pull_request_state.record["phase"] = json!("publication_prepared");
    pull_request_state.record["publication"] = json!({
        "kind": "update",
        "phase": "prepared",
        "previous_head": previous,
        "head": candidate.get("head").cloned().unwrap_or(Value::Null),
        "prepared_at": now,
        "operation": operation,
        "evidence": evidence
    });
    pull_request_state.task["candidate"] = candidate.get("head").cloned().unwrap_or(Value::Null);
    update_acceptance_reset(&mut pull_request_state.task);
    pull_request_state.task["pr"] = pull_request_state.record;
    store(s, pull_request_state.task, now, "pr_repair_update_prepared")
}

fn update_acceptance_reset(t: &mut J) {
    if let Some(acceptance) = model::get_mut(t, "acceptance").and_then(Value::as_object_mut) {
        update_acceptance_clear(acceptance);
    }
}

fn update_acceptance_clear(acceptance: &mut Map<String, Value>) {
    for criterion in acceptance.values_mut() {
        criterion["evidence"] = Value::Null;
    }
}

pub fn action(s: &mut J, command: &str, spec: &J, now: f64) -> Result<J> {
    if command == "pr-prepare" {
        return prepare(s, spec, now);
    }
    action_begin(s, command, spec, now)
}

fn action_begin(s: &mut J, command: &str, spec: &J, now: f64) -> Result<J> {
    if command == "pr-begin" {
        return publication_begin(s, spec, now);
    }
    action_reconcile(s, command, spec, now)
}

fn action_reconcile(s: &mut J, command: &str, spec: &J, now: f64) -> Result<J> {
    if command == "pr-reconcile" {
        return reconcile(s, spec, now, None);
    }
    action_observe(s, command, spec, now)
}

fn action_observe(s: &mut J, command: &str, spec: &J, now: f64) -> Result<J> {
    if command == "pr-observe" {
        return observe(s, spec, now, None);
    }
    action_update(s, command, spec, now)
}

fn action_update(s: &mut J, command: &str, spec: &J, now: f64) -> Result<J> {
    if command == "pr-update" {
        return update(s, spec, now);
    }
    action_unknown(command)
}

fn action_unknown(command: &str) -> Result<J> {
    Err(FlowError::progress(format!("unknown PR action: {command}")))
}

#[allow(dead_code)]
fn _unused_map() -> Map<String, Value> {
    Map::new()
}
