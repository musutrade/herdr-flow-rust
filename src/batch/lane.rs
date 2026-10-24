//! V2 bounded batching in `queue.json`, using V1 ownership and result
//! application. One managed task owns a repository/target lane. One active
//! batch and one bounded pending list are durable; a confirmed landing
//! atomically prepares its successor.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::batch::{gate, git, validation};
use crate::ci::{GitHub, GitHubApi};
use crate::error::{FlowError, Result};
use crate::model::{self, J};
use crate::pr as ledger;
use crate::progress;
use crate::util;
use crate::util::pyjson::{sha256_json_sorted, uuid_hex};

struct RepairRef {
    actual: String,
    parent: String,
    head: String,
}

struct RepairBinding {
    expected: J,
    token: String,
}

struct RepairRange {
    parent: String,
    head: String,
    evidence: String,
}

struct RepairAssignment {
    operation: String,
    record: J,
}

struct BatchLanding {
    pull_request: J,
    landing: J,
}

struct SnapshotPublication {
    own_commits: Vec<String>,
    previous_pr: Option<J>,
}

struct SnapshotStatus {
    root: String,
    status: String,
}

struct CandidateSource {
    bare: PathBuf,
    checkout: PathBuf,
    head: String,
}

struct DependencyTask {
    name: String,
    task: J,
}

struct OfferedSource {
    source: J,
    checks: Vec<J>,
}

struct LaneState {
    task: J,
    lane: J,
}

struct BatchState {
    task: J,
    lane: J,
    batch: J,
}

fn idle(t: &J) -> Result<()> {
    validation::idle(t)
}

fn mutable(s: &J, spec: &J) -> Result<(J, J)> {
    let t = ledger::mutable(s, spec)?;
    let lane = model::get(&t, "lane")
        .filter(|value| model::truthy(Some(value)))
        .cloned()
        .ok_or_else(|| FlowError::progress("task has no batch lane"))?;
    Ok((t, lane))
}

fn save(s: &mut J, t: J, now: f64, action: &str) -> Result<J> {
    ledger::store(s, t, now, action)
}

fn block(t: &mut J, reason: &str, condition: &str) {
    t["phase"] = json!("blocked");
    t["next_action"] = json!({
        "kind": "blocked",
        "owner": t.get("owner").cloned().unwrap_or(Value::Null),
        "reason": reason,
        "condition": condition
    });
}

fn candidate(t: &mut J, head: &str) -> Result<()> {
    idle(t)?;
    if model::string(t, "candidate").as_deref() != Some(head) {
        t["candidate"] = json!(head);
        if let Some(acceptance) = model::get_mut(t, "acceptance").and_then(Value::as_object_mut) {
            for item in acceptance.values_mut() {
                item["evidence"] = Value::Null;
            }
        }
    }
    Ok(())
}

fn lane_target(spec: &J) -> Result<(String, String, String)> {
    let repo = progress::text(model::get(spec, "repo"), "owner/repo", 250)?;
    let host = match model::get(spec, "host") {
        None => "github.com".to_string(),
        Some(value) => value
            .as_str()
            .map(ToString::to_string)
            .ok_or_else(|| FlowError::progress("invalid repository/host"))?,
    };
    if !regex::Regex::new(r"^[\w.-]+/[\w.-]+$")
        .expect("static repo regex")
        .is_match(&repo)
        || !regex::Regex::new(r"^[A-Za-z0-9.-]+$")
            .expect("static host regex")
            .is_match(&host)
    {
        return Err(FlowError::progress("invalid repository/host"));
    }
    let base_ref = progress::text(model::get(spec, "base_ref"), "target branch", 250)?;
    // check-ref-format is read-only and works without a checkout.
    git::output(
        Path::new("/"),
        &[
            "check-ref-format".to_string(),
            "--branch".to_string(),
            base_ref.clone(),
        ],
    )?;
    Ok((repo, host, base_ref))
}

fn lane_workspace(s: &J, spec: &J) -> Result<PathBuf> {
    let root = PathBuf::from(progress::text(
        model::get(spec, "workspace_root"),
        "integration workspace root",
        1000,
    )?);
    if !root.is_absolute() {
        return Err(FlowError::progress(
            "integration workspace root must be absolute",
        ));
    }
    let root = util::resolve_path(&root);
    if let Some(receiver) = model::get(s, "receiver").filter(|value| model::truthy(Some(value))) {
        if let Some(cwd) = model::string(receiver, "cwd") {
            if util::overlap(&root, Path::new(&cwd)) {
                return Err(FlowError::progress(
                    "keep integration workspaces outside the coordinator checkout",
                ));
            }
        }
    }
    Ok(root)
}

fn lane_capacity(spec: &J) -> Result<i64> {
    let capacity = match model::get(spec, "capacity") {
        None => 3,
        Some(value) => value
            .as_i64()
            .ok_or_else(|| FlowError::progress("pending batch capacity must be 1..16"))?,
    };
    if !(1..=16).contains(&capacity) {
        return Err(FlowError::progress("pending batch capacity must be 1..16"));
    }
    Ok(capacity)
}

fn lane_conflicts(s: &J, repo: &str, host: &str, base_ref: &str, root: &Path) -> Result<()> {
    let Some(tasks) = model::get(s, "tasks").and_then(Value::as_object) else {
        return Ok(());
    };
    tasks
        .values()
        .try_for_each(|other| lane_conflict(other, repo, host, base_ref, root))
}

fn lane_conflict(other: &J, repo: &str, host: &str, base_ref: &str, root: &Path) -> Result<()> {
    check_lane_owner(other, repo, host, base_ref)?;
    check_writer_overlap(other, root)
}

fn check_lane_owner(other: &J, repo: &str, host: &str, base_ref: &str) -> Result<()> {
    if lane_owned_by(other, repo, host, base_ref) {
        Err(FlowError::progress(
            "repository/target already has a lane owner in this queue",
        ))
    } else {
        Ok(())
    }
}

fn lane_owned_by(other: &J, repo: &str, host: &str, base_ref: &str) -> bool {
    model::get(other, "lane")
        .filter(|value| model::truthy(Some(value)))
        .map(|lane| lane_target_matches(lane, repo, host, base_ref))
        .unwrap_or(false)
}

fn lane_target_matches(lane: &J, repo: &str, host: &str, base_ref: &str) -> bool {
    [
        lane_repo_matches(lane, repo),
        lane_host_matches(lane, host),
        lane_base_matches(lane, base_ref),
    ]
    .into_iter()
    .all(|ok| ok)
}

fn lane_repo_matches(lane: &J, repo: &str) -> bool {
    model::string(lane, "repo").as_deref() == Some(repo)
}

fn lane_host_matches(lane: &J, host: &str) -> bool {
    model::string(lane, "host").as_deref() == Some(host)
}

fn lane_base_matches(lane: &J, base_ref: &str) -> bool {
    model::string(lane, "base_ref").as_deref() == Some(base_ref)
}

fn check_writer_overlap(other: &J, root: &Path) -> Result<()> {
    if writer_overlaps(other, root) {
        Err(FlowError::progress(
            "integration root overlaps an active developer workspace",
        ))
    } else {
        Ok(())
    }
}

fn writer_overlaps(other: &J, root: &Path) -> bool {
    model::get(other, "writer")
        .filter(|value| model::truthy(Some(value)))
        .and_then(|writer| model::string(writer, "workspace"))
        .map(|workspace| util::overlap(root, Path::new(&workspace)))
        .unwrap_or(false)
}

#[derive(Default)]
struct LaneInit {
    repo: String,
    host: String,
    base_ref: String,
    root: PathBuf,
    capacity: i64,
    gate: Value,
    max_repairs: i64,
}

pub fn init(s: &mut J, spec: &J, now: f64) -> Result<J> {
    let mut t = ledger::mutable(s, spec)?;
    progress::claim::lane_owner(s, &t)?;
    init_idle(s, spec, now, &mut t)
}

fn init_idle(s: &mut J, spec: &J, now: f64, t: &mut J) -> Result<J> {
    idle(t)?;
    init_unowned(s, spec, now, t)
}

fn init_unowned(s: &mut J, spec: &J, now: f64, t: &mut J) -> Result<J> {
    ensure_init_unowned(t)?;
    let mut setup = LaneInit::default();
    init_target(s, spec, now, t, &mut setup)
}

fn ensure_init_unowned(t: &J) -> Result<()> {
    if init_owned(t) {
        Err(FlowError::progress("task already owns a lane or a PR"))
    } else {
        Ok(())
    }
}

fn init_owned(t: &J) -> bool {
    [
        model::truthy(model::get(t, "lane")),
        model::truthy(model::get(t, "pr")),
    ]
    .into_iter()
    .any(|owned| owned)
}

fn init_target(s: &mut J, spec: &J, now: f64, t: &mut J, setup: &mut LaneInit) -> Result<J> {
    let (repo, host, base_ref) = lane_target(spec)?;
    setup.repo = repo;
    setup.host = host;
    setup.base_ref = base_ref;
    init_root(s, spec, now, t, setup)
}

fn init_root(s: &mut J, spec: &J, now: f64, t: &mut J, setup: &mut LaneInit) -> Result<J> {
    setup.root = lane_workspace(s, spec)?;
    init_capacity(s, spec, now, t, setup)
}

fn init_capacity(s: &mut J, spec: &J, now: f64, t: &mut J, setup: &mut LaneInit) -> Result<J> {
    setup.capacity = lane_capacity(spec)?;
    init_gate(s, spec, now, t, setup)
}

fn init_gate(s: &mut J, spec: &J, now: f64, t: &mut J, setup: &mut LaneInit) -> Result<J> {
    setup.gate = gate::validate(model::get(spec, "gate"))?;
    init_conflicts(s, spec, now, t, setup)
}

fn init_conflicts(s: &mut J, spec: &J, now: f64, t: &mut J, setup: &mut LaneInit) -> Result<J> {
    lane_conflicts(s, &setup.repo, &setup.host, &setup.base_ref, &setup.root)?;
    init_repairs(s, spec, now, t, setup)
}

fn init_repairs(s: &mut J, spec: &J, now: f64, t: &mut J, setup: &mut LaneInit) -> Result<J> {
    setup.max_repairs = init_max_repairs(spec);
    init_validate_repairs(s, spec, now, t, setup)
}

fn init_max_repairs(spec: &J) -> i64 {
    model::get(spec, "max_repairs")
        .map(|value| value.as_i64().unwrap_or(0))
        .unwrap_or(2)
}

fn init_validate_repairs(s: &mut J, spec: &J, now: f64, t: &mut J, setup: &LaneInit) -> Result<J> {
    if !(1..=10).contains(&setup.max_repairs) {
        return Err(FlowError::progress("batch max_repairs must be 1..10"));
    }
    init_store(s, spec, now, t, setup)
}

fn init_store(s: &mut J, spec: &J, now: f64, t: &mut J, setup: &LaneInit) -> Result<J> {
    t["lane"] = json!({
        "repo": setup.repo,
        "host": setup.host,
        "base_ref": setup.base_ref,
        "workspace_root": setup.root.display().to_string(),
        "capacity": setup.capacity,
        "gate": setup.gate,
        "pending": [],
        "active": null,
        "history": [],
        "sequence": 0,
        "older_batches": 0,
        "repairs": {},
        "max_repairs": setup.max_repairs,
        "gate_operations": {},
        "progress_timeout": progress::duration(model::get(spec, "progress_timeout_seconds"), Some(600.0))?
    });
    block(
        t,
        "lane waiting for completed tasks",
        "batch-offer followed by batch-freeze",
    );
    save(s, t.clone(), now, "batch_lane_registered")
}

fn validate_offer_member(t: &J, mt: &J, event: Option<&J>) -> Result<()> {
    if offer_member_ok(t, mt, event) {
        Ok(())
    } else {
        Err(FlowError::progress(
            "member must be an unowned, released task with its current results applied",
        ))
    }
}

fn offer_member_ok(t: &J, mt: &J, event: Option<&J>) -> bool {
    [
        offer_member_unowned(t, mt, event),
        !validation::unresolved_findings(mt),
    ]
    .into_iter()
    .all(|ok| ok)
}

fn offer_member_unowned(t: &J, mt: &J, event: Option<&J>) -> bool {
    [
        offer_task_unowned(t, mt),
        offer_no_delivery(mt),
        offer_no_pr(mt),
        offer_no_lane(mt),
        offer_control_running(mt),
        offer_phase_blocked(mt),
        offer_no_writer(mt),
        offer_pending_empty(mt),
        offer_event_ok(t, mt, event),
    ]
    .into_iter()
    .all(|ok| ok)
}

fn offer_task_unowned(t: &J, mt: &J) -> bool {
    model::string(mt, "task") != model::string(t, "task")
}

fn offer_no_delivery(mt: &J) -> bool {
    !model::truthy(model::get(mt, "batch_delivery"))
}

fn offer_no_pr(mt: &J) -> bool {
    !model::truthy(model::get(mt, "pr"))
}

fn offer_no_lane(mt: &J) -> bool {
    !model::truthy(model::get(mt, "lane"))
}

fn offer_control_running(mt: &J) -> bool {
    model::string(mt, "control").as_deref() == Some("running")
}

fn offer_phase_blocked(mt: &J) -> bool {
    model::string(mt, "phase").as_deref() == Some("blocked")
}

fn offer_no_writer(mt: &J) -> bool {
    !model::truthy(model::get(mt, "writer"))
}

fn offer_pending_empty(mt: &J) -> bool {
    model::array_field(mt, "pending")
        .map(Vec::is_empty)
        .unwrap_or(true)
}

fn offer_event_ok(_t: &J, mt: &J, event: Option<&J>) -> bool {
    event
        .map(|event| offer_event_applied(mt, event))
        .unwrap_or(false)
}

fn offer_event_applied(mt: &J, event: &J) -> bool {
    [
        model::bool_or(event, "applied", false),
        model::string(event, "outcome").as_deref() == Some("complete"),
        model::get(event, "binding")
            .map(|binding| progress::same(mt, binding))
            .unwrap_or(false),
    ]
    .into_iter()
    .all(|ok| ok)
}

fn offer_source(s: &J, lane: &J, t: &J, mt: &J, source: &J) -> Result<()> {
    ensure_offer_source_identity(source, mt, lane)?;
    offer_source_total(s, lane, t, source)
}

fn offer_source_total(s: &J, lane: &J, t: &J, source: &J) -> Result<()> {
    ensure_offer_source_total(lane, source)?;
    offer_source_claimed(s, lane, t, source)
}

fn offer_source_claimed(s: &J, lane: &J, t: &J, source: &J) -> Result<()> {
    ensure_offer_source_claimed(s, t, source)?;
    ensure_verifier_isolated(lane, source)
}

fn ensure_offer_source_identity(source: &J, mt: &J, lane: &J) -> Result<()> {
    if offer_source_identity_ok(source, mt, lane) {
        Ok(())
    } else {
        Err(FlowError::progress(
            "member candidate changed or integration root overlaps development",
        ))
    }
}

fn offer_source_identity_ok(source: &J, mt: &J, lane: &J) -> bool {
    [
        source_head_matches(source, mt),
        !source_overlaps_checkout(source, lane),
    ]
    .into_iter()
    .all(|ok| ok)
}

fn source_head_matches(source: &J, mt: &J) -> bool {
    model::string(source, "head") == model::string(mt, "candidate")
}

fn source_overlaps_checkout(source: &J, lane: &J) -> bool {
    util::overlap(
        Path::new(&model::string(source, "checkout").unwrap_or_default()),
        Path::new(&model::string(lane, "workspace_root").unwrap_or_default()),
    )
}

fn ensure_offer_source_total(lane: &J, source: &J) -> Result<()> {
    if source_total(lane, source) > 256 {
        Err(FlowError::progress(
            "pending batch would exceed the 256 source-commit provenance limit",
        ))
    } else {
        Ok(())
    }
}

fn source_total(lane: &J, source: &J) -> usize {
    source_commit_count(source) + pending_commit_count(lane)
}

fn source_commit_count(value: &J) -> usize {
    model::array_field(value, "own_commits")
        .map(Vec::len)
        .unwrap_or(0)
}

fn pending_commit_count(lane: &J) -> usize {
    model::array_field(lane, "pending")
        .map(|items| items.iter().map(source_commit_count).sum())
        .unwrap_or(0)
}

fn ensure_offer_source_claimed(s: &J, t: &J, source: &J) -> Result<()> {
    if source_claimed(s, t, source) {
        Err(FlowError::progress(
            "source range includes commits already owned by another batch member",
        ))
    } else {
        Ok(())
    }
}

fn source_claimed(s: &J, t: &J, source: &J) -> bool {
    let claimed = claimed_commits(s, &model::string(t, "task").unwrap_or_default());
    model::array_field(source, "own_commits")
        .map(|commits| {
            commits
                .iter()
                .filter_map(Value::as_str)
                .any(|commit| claimed.contains(commit))
        })
        .unwrap_or(false)
}

fn claimed_commits(s: &J, owner: &str) -> BTreeSet<String> {
    model::get(s, "tasks")
        .and_then(Value::as_object)
        .map(|tasks| {
            tasks
                .values()
                .flat_map(|task| delivery_commits(task, owner))
                .collect()
        })
        .unwrap_or_default()
}

fn delivery_commits(task: &J, owner: &str) -> Vec<String> {
    model::get(task, "batch_delivery")
        .filter(|delivery| model::string(delivery, "owner").as_deref() == Some(owner))
        .map(delivery_commit_strings)
        .unwrap_or_default()
}

fn delivery_commit_strings(delivery: &J) -> Vec<String> {
    model::array_field(delivery, "own_commits")
        .map(|commits| {
            commits
                .iter()
                .filter_map(Value::as_str)
                .map(ToString::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn ensure_verifier_isolated(lane: &J, source: &J) -> Result<()> {
    if verifier_from_checkout(lane, source) {
        Err(FlowError::progress(
            "trusted gate verifier must not be supplied by a member checkout",
        ))
    } else {
        Ok(())
    }
}

fn verifier_from_checkout(lane: &J, source: &J) -> bool {
    let checkout = model::string(source, "checkout").unwrap_or_default();
    model::get(lane, "gate")
        .and_then(|gate| model::array_field(gate, "verifier"))
        .into_iter()
        .flatten()
        .any(|argument| verifier_argument_in(argument, &checkout))
}

fn verifier_argument_in(argument: &J, checkout: &str) -> bool {
    if checkout.is_empty() {
        return false;
    }
    argument
        .as_str()
        .map(|argument| {
            let argument = util::resolve_path(Path::new(argument));
            let checkout = util::resolve_path(Path::new(checkout));
            argument.is_absolute() && util::is_relative_to(&argument, &checkout)
        })
        .unwrap_or(false)
}

fn offer_checks(m: &J, mt: &J) -> Result<Vec<J>> {
    let Some(checks) = model::get(m, "acceptance").and_then(Value::as_array) else {
        return Err(FlowError::progress(
            "member must map to explicit original acceptance IDs",
        ));
    };
    let unique: BTreeSet<&str> = checks.iter().filter_map(Value::as_str).collect();
    let accepted: BTreeSet<&str> = model::get(mt, "acceptance")
        .and_then(Value::as_object)
        .map(|acceptance| acceptance.keys().map(String::as_str).collect())
        .unwrap_or_default();
    if checks.is_empty()
        || unique.len() != checks.len()
        || checks.iter().any(|key| {
            key.as_str()
                .map(|key| !accepted.contains(key))
                .unwrap_or(true)
        })
    {
        return Err(FlowError::progress(
            "member must map to explicit original acceptance IDs",
        ));
    }
    Ok(checks.clone())
}

fn offer_dependencies(
    s: &J,
    lane: &J,
    source: &J,
    mt: &J,
    deps: Option<&Vec<J>>,
) -> Result<Vec<J>> {
    let Some(deps) = deps else {
        return Err(FlowError::progress(
            "member requires explicit dependencies (max 32)",
        ));
    };
    ensure_dependency_count(deps)?;
    offer_dependencies_list(s, lane, source, mt, deps)
}

fn ensure_dependency_count(deps: &[J]) -> Result<()> {
    if deps.len() > 32 {
        Err(FlowError::progress(
            "member requires explicit dependencies (max 32)",
        ))
    } else {
        Ok(())
    }
}

fn offer_dependencies_list(s: &J, lane: &J, source: &J, mt: &J, deps: &[J]) -> Result<Vec<J>> {
    let accepted = dependency_accepted(lane);
    let mut resolved = Vec::new();
    deps.iter().try_for_each(|dep| {
        dependency_collect(s, lane, source, mt, dep, &accepted, &mut resolved)
    })?;
    Ok(resolved)
}

fn dependency_collect(
    s: &J,
    lane: &J,
    source: &J,
    mt: &J,
    dep: &J,
    accepted: &std::collections::BTreeMap<String, J>,
    resolved: &mut Vec<J>,
) -> Result<()> {
    let item = offer_dependency(s, lane, source, mt, dep, accepted, resolved)?;
    resolved.push(item);
    Ok(())
}

fn dependency_accepted(lane: &J) -> std::collections::BTreeMap<String, J> {
    let mut accepted = pending_dependencies(lane);
    extend_active_dependencies(lane, &mut accepted);
    accepted
}

fn pending_dependencies(lane: &J) -> std::collections::BTreeMap<String, J> {
    model::array_field(lane, "pending")
        .map(|items| {
            items
                .iter()
                .filter_map(|item| model::string(item, "task").map(|task| (task, item.clone())))
                .collect()
        })
        .unwrap_or_default()
}

fn extend_active_dependencies(lane: &J, accepted: &mut std::collections::BTreeMap<String, J>) {
    if let Some(active) = model::get(lane, "active").filter(|value| model::truthy(Some(value))) {
        extend_member_dependencies(active, accepted);
    }
}

fn extend_member_dependencies(active: &J, accepted: &mut std::collections::BTreeMap<String, J>) {
    let members = model::array_field(active, "members")
        .cloned()
        .unwrap_or_default();
    members
        .iter()
        .for_each(|member| dependency_member_insert(member, accepted));
}

fn dependency_member_insert(member: &J, accepted: &mut std::collections::BTreeMap<String, J>) {
    if let Some(task) = model::string(member, "task") {
        accepted.insert(task, member.clone());
    }
}

fn offer_dependency(
    s: &J,
    lane: &J,
    source: &J,
    mt: &J,
    dep: &J,
    accepted: &std::collections::BTreeMap<String, J>,
    resolved: &[J],
) -> Result<J> {
    let _ = progress::object(dep, "batch dependency")?;
    dependency_task(s, lane, source, mt, dep, accepted, resolved)
}

fn dependency_task(
    s: &J,
    lane: &J,
    source: &J,
    mt: &J,
    dep: &J,
    accepted: &std::collections::BTreeMap<String, J>,
    resolved: &[J],
) -> Result<J> {
    let dep_task = model::string(dep, "task").ok_or_else(|| {
        FlowError::progress(
            "dependency must be accepted earlier in this lane or have a confirmed landing",
        )
    })?;
    let dt = model::dig(s, &["tasks", &dep_task])
        .cloned()
        .unwrap_or_else(|| json!({}));
    dependency_head(
        lane,
        source,
        mt,
        dep,
        accepted,
        resolved,
        DependencyTask {
            name: dep_task,
            task: dt,
        },
    )
}

fn dependency_head(
    lane: &J,
    source: &J,
    mt: &J,
    dep: &J,
    accepted: &std::collections::BTreeMap<String, J>,
    resolved: &[J],
    dependency_task: DependencyTask,
) -> Result<J> {
    let state = dependency_state(dependency_task.task, accepted, &dependency_task.name)?;
    dependency_sha(lane, source, mt, dep, state, resolved)
}

struct DependencyState {
    dt: J,
    source_head: String,
    landing: Option<J>,
    prior: Option<J>,
}

fn dependency_state(
    dt: J,
    accepted: &std::collections::BTreeMap<String, J>,
    dep_task: &str,
) -> Result<DependencyState> {
    let delivery = model::get(&dt, "batch_delivery")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let prior = accepted.get(dep_task).cloned();
    let pr = model::get(&dt, "pr").cloned().unwrap_or_else(|| json!({}));
    let source_head = dependency_source_head(&prior, &delivery, &pr);
    let Some(source_head) = source_head else {
        return Err(FlowError::progress(
            "dependency must be accepted earlier in this lane or have a confirmed landing",
        ));
    };
    let landing = dependency_landing(&delivery, &pr);
    Ok(DependencyState {
        dt,
        source_head,
        landing,
        prior,
    })
}

fn dependency_source_head(prior: &Option<J>, delivery: &J, pr: &J) -> Option<String> {
    prior
        .as_ref()
        .and_then(|prior| model::string(prior, "head"))
        .or_else(|| model::string(delivery, "head"))
        .or_else(|| model::string(pr, "head"))
}

fn dependency_landing(delivery: &J, pr: &J) -> Option<J> {
    model::get(delivery, "landing")
        .filter(|value| model::truthy(Some(value)))
        .cloned()
        .or_else(|| {
            model::get(pr, "landing")
                .filter(|value| model::truthy(Some(value)))
                .cloned()
        })
}

fn dependency_sha(
    lane: &J,
    source: &J,
    mt: &J,
    dep: &J,
    state: DependencyState,
    resolved: &[J],
) -> Result<J> {
    let dep_head = ledger::sha(model::get(dep, "head"))?;
    dependency_verify_valid(lane, source, mt, state, dep_head, resolved)
}

fn dependency_verify_valid(
    lane: &J,
    source: &J,
    mt: &J,
    state: DependencyState,
    dep_head: String,
    resolved: &[J],
) -> Result<J> {
    if dependency_valid(&state, mt, &dep_head, resolved) {
        dependency_after_valid(lane, source, state)
    } else {
        Err(FlowError::progress(
            "dependency must be accepted earlier in this lane or have a confirmed landing",
        ))
    }
}

fn dependency_valid(state: &DependencyState, mt: &J, dep_head: &str, resolved: &[J]) -> bool {
    [
        state.source_head == dep_head,
        model::string(&state.dt, "candidate").as_deref() == Some(state.source_head.as_str()),
        model::string(&state.dt, "control").as_deref() != Some("cancelled"),
        model::string(&state.dt, "task") != model::string(mt, "task"),
        !dependency_duplicate(&state.dt, resolved),
        state.prior.is_some() || state.landing.is_some(),
    ]
    .into_iter()
    .all(|ok| ok)
}

fn dependency_duplicate(dt: &J, resolved: &[J]) -> bool {
    resolved
        .iter()
        .any(|item| model::string(item, "task") == model::string(dt, "task"))
}

fn dependency_after_valid(lane: &J, source: &J, state: DependencyState) -> Result<J> {
    dependency_verify_landing(lane, &state)?;
    dependency_push(source, state)
}

fn dependency_verify_landing(lane: &J, state: &DependencyState) -> Result<()> {
    if dependency_landing_wrong(lane, &state.landing) {
        Err(FlowError::progress(
            "dependency landed on another repository/target",
        ))
    } else {
        Ok(())
    }
}

fn dependency_landing_wrong(lane: &J, landing: &Option<J>) -> bool {
    landing
        .as_ref()
        .map(|landing| dependency_landing_other(lane, landing))
        .unwrap_or(false)
}

fn dependency_landing_other(lane: &J, landing: &J) -> bool {
    [
        model::string(landing, "repo") != model::string(lane, "repo"),
        model::string(landing, "base_ref") != model::string(lane, "base_ref"),
    ]
    .into_iter()
    .any(|wrong| wrong)
}

fn dependency_push(source: &J, state: DependencyState) -> Result<J> {
    dependency_merge_base(source, &state.source_head)?;
    Ok(json!({
        "task": state.dt.get("task").cloned().unwrap_or(Value::Null),
        "head": state.source_head,
        "landing": state.landing
    }))
}

fn dependency_merge_base(source: &J, source_head: &str) -> Result<()> {
    ledger::git(
        Path::new(&model::string(source, "checkout").unwrap_or_default()),
        &[
            "merge-base",
            "--is-ancestor",
            source_head,
            &model::string(source, "original_base").unwrap_or_default(),
        ],
    )?;
    Ok(())
}

pub fn offer(s: &mut J, spec: &J, now: f64) -> Result<J> {
    let mut staged = s.clone();
    let result = offer_staged(&mut staged, spec, now)?;
    *s = staged;
    Ok(result)
}

fn offer_staged(s: &mut J, spec: &J, now: f64) -> Result<J> {
    let (t, lane) = mutable(s, spec)?;
    offer_idle(s, spec, now, t, lane)
}

fn offer_idle(s: &mut J, spec: &J, now: f64, t: J, lane: J) -> Result<J> {
    idle(&t)?;
    offer_capacity(s, spec, now, t, lane)
}

fn offer_capacity(s: &mut J, spec: &J, now: f64, t: J, lane: J) -> Result<J> {
    ensure_offer_capacity(&lane)?;
    offer_member(s, spec, now, t, lane)
}

fn ensure_offer_capacity(lane: &J) -> Result<()> {
    if offer_capacity_full(lane) {
        Err(FlowError::progress(
            "pending batch is full; keep the completed task at its existing handoff",
        ))
    } else {
        Ok(())
    }
}

fn offer_capacity_full(lane: &J) -> bool {
    offer_pending_len(lane) >= offer_lane_capacity(lane)
}

fn offer_pending_len(lane: &J) -> usize {
    model::array_field(lane, "pending")
        .map(Vec::len)
        .unwrap_or(0)
}

fn offer_lane_capacity(lane: &J) -> usize {
    model::int(lane, "capacity").unwrap_or(3) as usize
}

fn offer_member(s: &mut J, spec: &J, now: f64, t: J, lane: J) -> Result<J> {
    let m = progress::object_owned(model::get(spec, "member"), "completed member")?;
    offer_member_task(s, now, t, lane, m)
}

fn offer_member_task(s: &mut J, now: f64, t: J, lane: J, m: J) -> Result<J> {
    let mt = progress::task(s, model::get(&m, "task"), model::get(&m, "state_version"))?;
    progress::claim::offer(s, &t, &mt)?;
    offer_member_event(s, now, t, lane, m, mt)
}

fn offer_member_event(s: &mut J, now: f64, t: J, lane: J, m: J, mt: J) -> Result<J> {
    let event =
        model::string(&m, "event").and_then(|event| model::dig(s, &["events", &event]).cloned());
    validate_offer_member(&t, &mt, event.as_ref())?;
    offer_member_source(s, now, t, lane, m, mt)
}

fn offer_member_source(s: &mut J, now: f64, t: J, lane: J, m: J, mt: J) -> Result<J> {
    let source = ledger::provenance(&offer_source_spec(&m, &lane))?;
    offer_member_source_check(s, now, t, lane, m, mt, source)
}

fn offer_source_spec(m: &J, lane: &J) -> J {
    json!({
        "checkout": m.get("checkout").cloned().unwrap_or(Value::Null),
        "repo": lane.get("repo").cloned().unwrap_or(Value::Null),
        "host": lane.get("host").cloned().unwrap_or(Value::Null),
        "original_base": m.get("original_base").cloned().unwrap_or(Value::Null),
        "head": m.get("head").cloned().unwrap_or(Value::Null),
        "own_commits": m.get("own_commits").cloned().unwrap_or(Value::Null)
    })
}

fn offer_member_source_check(
    s: &mut J,
    now: f64,
    t: J,
    lane: J,
    m: J,
    mt: J,
    source: J,
) -> Result<J> {
    offer_source(s, &lane, &t, &mt, &source)?;
    offer_member_acceptance(s, now, t, lane, m, mt, source)
}

fn offer_member_acceptance(
    s: &mut J,
    now: f64,
    t: J,
    lane: J,
    m: J,
    mt: J,
    source: J,
) -> Result<J> {
    let checks = offer_checks(&m, &mt)?;
    offer_member_dependencies(
        s,
        now,
        LaneState { task: t, lane },
        m,
        mt,
        OfferedSource { source, checks },
    )
}

fn offer_member_dependencies(
    s: &mut J,
    now: f64,
    lane_state: LaneState,
    m: J,
    mt: J,
    offered_source: OfferedSource,
) -> Result<J> {
    let dependencies = offer_dependencies(
        s,
        &lane_state.lane,
        &offered_source.source,
        &mt,
        model::get(&m, "dependencies").and_then(Value::as_array),
    )?;
    offer_member_evidence(s, now, lane_state, m, mt, offered_source, dependencies)
}

fn offer_member_evidence(
    s: &mut J,
    now: f64,
    lane_state: LaneState,
    m: J,
    mt: J,
    offered_source: OfferedSource,
    dependencies: Vec<J>,
) -> Result<J> {
    let evidence = progress::text(
        model::get(&m, "evidence"),
        "completed task handoff evidence",
        1000,
    )?;
    offer_member_store(
        s,
        now,
        lane_state,
        mt,
        offered_source,
        dependencies,
        evidence,
    )
}

fn offer_member_store(
    s: &mut J,
    now: f64,
    mut lane_state: LaneState,
    mut mt: J,
    offered_source: OfferedSource,
    dependencies: Vec<J>,
    evidence: String,
) -> Result<J> {
    let mut member = offer_member_value(
        &mt,
        &offered_source.source,
        offered_source.checks,
        dependencies,
        &evidence,
        now,
    );
    // Intake identity is independent of later batch IDs, freezes and landings.
    member["receipt_id"] = json!(uuid_hex());
    offer_member_push(&mut lane_state.lane, &member);
    let owner = model::string(&lane_state.task, "task").unwrap_or_default();
    mt["batch_delivery"] = json!({
        "owner": owner,
        "receipt_id": member["receipt_id"],
        "head": offered_source.source.get("head").cloned().unwrap_or(Value::Null),
        "own_commits": offered_source.source.get("own_commits").cloned().unwrap_or(Value::Null),
        "phase": "accepted",
        "accepted_at": now
    });
    save(s, mt, now, "batch_member_accepted")?;
    offer_commit(s, lane_state.task, now, lane_state.lane)
}

fn offer_member_value(
    mt: &J,
    source: &J,
    checks: Vec<J>,
    dependencies: Vec<J>,
    evidence: &str,
    now: f64,
) -> J {
    let mut member = source.clone();
    member["task"] = mt["task"].clone();
    member["epoch"] = mt["epoch"].clone();
    member["acceptance"] = json!(checks);
    member["dependencies"] = json!(dependencies);
    member["evidence"] = json!(evidence);
    member["accepted_at"] = json!(now);
    member
}

fn offer_member_push(lane: &mut J, member: &J) {
    if let Some(pending_items) = model::get_mut(lane, "pending").and_then(Value::as_array_mut) {
        pending_items.push(member.clone());
    }
}

fn offer_commit(s: &mut J, mut t: J, now: f64, lane: J) -> Result<J> {
    t["lane"] = lane;
    save(s, t, now, "batch_offer_accepted")
}

fn freeze_next(s: &mut J, t: J, lane: J, now: f64, api: &dyn GitHubApi) -> Result<J> {
    idle(&t)?;
    freeze_ready(s, now, api, t, lane)
}

fn freeze_ready(s: &mut J, now: f64, api: &dyn GitHubApi, t: J, lane: J) -> Result<J> {
    ensure_freeze_ready(&lane)?;
    freeze_sequence(s, now, api, t, lane)
}

fn ensure_freeze_ready(lane: &J) -> Result<()> {
    if freeze_blocked(lane) {
        Err(FlowError::progress(
            "freeze requires an empty publication slot and at least one pending task",
        ))
    } else {
        Ok(())
    }
}

fn freeze_blocked(lane: &J) -> bool {
    [
        model::truthy(model::get(lane, "active")),
        model::array_field(lane, "pending")
            .map(Vec::is_empty)
            .unwrap_or(true),
    ]
    .into_iter()
    .any(|blocked| blocked)
}

fn freeze_sequence(s: &mut J, now: f64, api: &dyn GitHubApi, t: J, lane: J) -> Result<J> {
    let sequence = model::int(&lane, "sequence").unwrap_or(0);
    if sequence >= 128 {
        return Err(FlowError::progress(
            "lane reached 128 batches; preserve it and establish a new bounded task contract",
        ));
    }
    freeze_base(s, now, api, t, lane, sequence)
}

fn freeze_base(
    s: &mut J,
    now: f64,
    api: &dyn GitHubApi,
    t: J,
    lane: J,
    sequence: i64,
) -> Result<J> {
    let base = validation::latest(&lane, api)?;
    freeze_ancestry(s, now, api, t, lane, sequence, base)
}

fn freeze_ancestry(
    s: &mut J,
    now: f64,
    api: &dyn GitHubApi,
    t: J,
    lane: J,
    sequence: i64,
    base: String,
) -> Result<J> {
    validation::landing_ancestry(&lane, &base, api)?;
    freeze_build(s, now, api, t, lane, sequence, base)
}

fn freeze_build(
    s: &mut J,
    now: f64,
    api: &dyn GitHubApi,
    t: J,
    mut lane: J,
    sequence: i64,
    base: String,
) -> Result<J> {
    lane["sequence"] = json!(sequence + 1);
    let ident = format!("batch-{}-{}", sequence + 1, &uuid_hex()[..12]);
    let pending = model::array_field(&lane, "pending")
        .cloned()
        .unwrap_or_default();
    let batch = json!({
        "id": ident,
        "base": base,
        "members": pending,
        "phase": "integration_prepared",
        "frozen_at": now,
        "workspace": Path::new(&model::string(&lane, "workspace_root").unwrap_or_default()).join(&ident).display().to_string(),
        "snapshot": null,
        "gate": null,
        "revision": 0,
        "progress_deadline": now + model::number(&lane, "progress_timeout").unwrap_or(600.0),
        "integration": {"ready": false, "offset": 0, "mapping": [], "current": null, "conflict": null},
        "corrections": [],
        "versions": []
    });
    freeze_dependencies(
        s,
        now,
        api,
        BatchState {
            task: t,
            lane,
            batch,
        },
    )
}

fn freeze_dependencies(
    s: &mut J,
    now: f64,
    api: &dyn GitHubApi,
    batch_state: BatchState,
) -> Result<J> {
    validation::dependencies(s, &batch_state.lane, &batch_state.batch, api)?;
    freeze_activate(s, now, batch_state)
}

fn freeze_activate(s: &mut J, now: f64, mut batch_state: BatchState) -> Result<J> {
    batch_state.lane["active"] = batch_state.batch.clone();
    batch_state.lane["pending"] = json!([]);
    let members = model::array_field(&batch_state.batch, "members")
        .cloned()
        .unwrap_or_default();
    members
        .iter()
        .try_for_each(|member| freeze_member(s, member, &batch_state.batch, now))?;
    freeze_finish(s, now, batch_state)
}

fn freeze_member(s: &mut J, member: &J, batch: &J, now: f64) -> Result<()> {
    let task_name = model::string(member, "task").unwrap_or_default();
    let mut mt = model::dig(s, &["tasks", &task_name])
        .cloned()
        .unwrap_or_else(|| json!({}));
    mt["batch_delivery"]["phase"] = json!("frozen");
    mt["batch_delivery"]["batch"] = batch["id"].clone();
    mt["batch_delivery"]["base"] = batch["base"].clone();
    save(s, mt, now, "batch_member_frozen")?;
    Ok(())
}

fn freeze_finish(s: &mut J, now: f64, mut batch_state: BatchState) -> Result<J> {
    candidate(
        &mut batch_state.task,
        &model::string(&batch_state.batch, "base").unwrap_or_default(),
    )?;
    block(
        &mut batch_state.task,
        "frozen batch ready for isolated integration",
        &format!(
            "batch-integrate for {}",
            model::string(&batch_state.batch, "id").unwrap_or_default()
        ),
    );
    batch_state.task["lane"] = batch_state.lane;
    save(s, batch_state.task, now, "batch_members_and_base_frozen")
}

pub fn freeze(s: &mut J, spec: &J, now: f64, api: Option<&dyn GitHubApi>) -> Result<J> {
    let (t, lane) = mutable(s, spec)?;
    let owned;
    let api: &dyn GitHubApi = match api {
        Some(api) => api,
        None => {
            owned = GitHub::new(lane.clone());
            &owned
        }
    };
    freeze_next(s, t, lane, now, api)
}

fn active(lane: &J, spec: &J) -> Result<J> {
    let batch = model::get(lane, "active")
        .filter(|value| model::truthy(Some(value)))
        .cloned()
        .ok_or_else(|| FlowError::progress("exact active batch ID required"))?;
    if model::string(spec, "batch") != model::string(&batch, "id") {
        return Err(FlowError::progress("exact active batch ID required"));
    }
    Ok(batch)
}

pub fn integrate(
    s: &mut J,
    spec: &J,
    now: f64,
    checkpoint: &mut dyn FnMut(&mut J) -> Result<()>,
    api: Option<&dyn GitHubApi>,
) -> Result<J> {
    let (t, lane) = mutable(s, spec)?;
    integrate_idle(s, spec, now, checkpoint, api, t, lane)
}

fn integrate_idle(
    s: &mut J,
    spec: &J,
    now: f64,
    checkpoint: &mut dyn FnMut(&mut J) -> Result<()>,
    api: Option<&dyn GitHubApi>,
    t: J,
    lane: J,
) -> Result<J> {
    idle(&t)?;
    integrate_active(s, spec, now, checkpoint, api, t, lane)
}

fn integrate_active(
    s: &mut J,
    spec: &J,
    now: f64,
    checkpoint: &mut dyn FnMut(&mut J) -> Result<()>,
    api: Option<&dyn GitHubApi>,
    t: J,
    lane: J,
) -> Result<J> {
    let batch = active(&lane, spec)?;
    integrate_phase(
        s,
        now,
        checkpoint,
        api,
        BatchState {
            task: t,
            lane,
            batch,
        },
    )
}

fn integrate_phase(
    s: &mut J,
    now: f64,
    checkpoint: &mut dyn FnMut(&mut J) -> Result<()>,
    api: Option<&dyn GitHubApi>,
    batch_state: BatchState,
) -> Result<J> {
    if integrate_ready(&batch_state.batch) {
        integrate_api(s, now, checkpoint, api, batch_state)
    } else {
        Err(FlowError::progress(
            "batch is not ready for integration; inspect its current phase",
        ))
    }
}

fn integrate_ready(batch: &J) -> bool {
    [
        integrate_phase_ok(batch),
        model::get(batch, "repair_update").is_none(),
    ]
    .into_iter()
    .all(|ok| ok)
}

fn integrate_phase_ok(batch: &J) -> bool {
    matches!(
        model::string(batch, "phase").as_deref(),
        Some("integration_prepared" | "integrating")
    )
}

fn integrate_api(
    s: &mut J,
    now: f64,
    checkpoint: &mut dyn FnMut(&mut J) -> Result<()>,
    api: Option<&dyn GitHubApi>,
    batch_state: BatchState,
) -> Result<J> {
    let owned;
    let api: &dyn GitHubApi = if let Some(api) = api {
        api
    } else {
        owned = GitHub::new(batch_state.lane.clone());
        &owned
    };
    integrate_deps(s, now, checkpoint, batch_state, api)
}

fn integrate_deps(
    s: &mut J,
    now: f64,
    checkpoint: &mut dyn FnMut(&mut J) -> Result<()>,
    batch_state: BatchState,
    api: &dyn GitHubApi,
) -> Result<J> {
    validation::dependencies(s, &batch_state.lane, &batch_state.batch, api)?;
    integrate_latest(s, now, checkpoint, batch_state, api)
}

fn integrate_latest(
    s: &mut J,
    now: f64,
    checkpoint: &mut dyn FnMut(&mut J) -> Result<()>,
    batch_state: BatchState,
    api: &dyn GitHubApi,
) -> Result<J> {
    let latest = validation::latest(&batch_state.lane, api)?;
    integrate_target(s, now, checkpoint, batch_state, &latest)
}

fn integrate_target(
    s: &mut J,
    now: f64,
    checkpoint: &mut dyn FnMut(&mut J) -> Result<()>,
    batch_state: BatchState,
    latest: &str,
) -> Result<J> {
    if latest != model::string(&batch_state.batch, "base").unwrap_or_default() {
        return Err(FlowError::progress(
            "target changed before integration; explicitly rebase this frozen batch",
        ));
    }
    integrate_run(s, now, checkpoint, batch_state)
}

fn integrate_run(
    s: &mut J,
    now: f64,
    checkpoint: &mut dyn FnMut(&mut J) -> Result<()>,
    batch_state: BatchState,
) -> Result<J> {
    let BatchState {
        task: mut t,
        lane,
        mut batch,
    } = batch_state;

    batch["phase"] = json!("integrating");
    let mut persist = |batch: &mut J| -> Result<()> {
        if model::string(batch, "phase").as_deref() == Some("integration_conflict") {
            let conflict_parent =
                model::dig_str(batch, &["integration", "conflict", "parent"]).unwrap_or_default();
            candidate(&mut t, &conflict_parent)?;
            batch["failure_token"] = json!(repair_token(batch));
            block(
                &mut t,
                "frozen task changes conflict",
                "one V1 repair assignment bound to this batch conflict",
            );
        }
        let task_name = model::string(&t, "task").unwrap_or_default();
        t["lane"]["active"] = batch.clone();
        s["tasks"][&task_name] = t.clone();
        t = save(s, t.clone(), now, "batch_integration_checkpoint")?;
        checkpoint(s)
    };
    persist(&mut batch)?;
    let result = git::integrate(&mut batch, &lane, &mut persist);
    // Persist the final integration state even after a conflict return.
    t["lane"]["active"] = batch.clone();
    let task_name = model::string(&t, "task").unwrap_or_default();
    s["tasks"][&task_name] = t.clone();
    result?;
    Ok(t)
}

pub fn snapshot(s: &mut J, spec: &J, now: f64, api: Option<&dyn GitHubApi>) -> Result<J> {
    let (t, lane) = mutable(s, spec)?;
    snapshot_idle(s, spec, now, api, t, lane)
}

fn snapshot_idle(
    s: &mut J,
    spec: &J,
    now: f64,
    api: Option<&dyn GitHubApi>,
    t: J,
    lane: J,
) -> Result<J> {
    idle(&t)?;
    snapshot_active(s, spec, now, api, t, lane)
}

fn snapshot_active(
    s: &mut J,
    spec: &J,
    now: f64,
    api: Option<&dyn GitHubApi>,
    t: J,
    lane: J,
) -> Result<J> {
    let batch = active(&lane, spec)?;
    snapshot_phase(
        s,
        spec,
        now,
        api,
        BatchState {
            task: t,
            lane,
            batch,
        },
    )
}

fn snapshot_phase(
    s: &mut J,
    spec: &J,
    now: f64,
    api: Option<&dyn GitHubApi>,
    batch_state: BatchState,
) -> Result<J> {
    if snapshot_ready(&batch_state.batch) {
        snapshot_api(s, spec, now, api, batch_state)
    } else {
        Err(FlowError::progress(
            "freeze the final snapshot only after all frozen members are integrated",
        ))
    }
}

fn snapshot_ready(batch: &J) -> bool {
    [
        model::string(batch, "phase").as_deref() == Some("integrated"),
        model::get(batch, "repair_update").is_none(),
    ]
    .into_iter()
    .all(|ok| ok)
}

fn snapshot_api(
    s: &mut J,
    spec: &J,
    now: f64,
    api: Option<&dyn GitHubApi>,
    batch_state: BatchState,
) -> Result<J> {
    let owned;
    let api: &dyn GitHubApi = if let Some(api) = api {
        api
    } else {
        owned = GitHub::new(batch_state.lane.clone());
        &owned
    };
    snapshot_deps(s, spec, now, batch_state, api)
}

fn snapshot_deps(
    s: &mut J,
    spec: &J,
    now: f64,
    batch_state: BatchState,
    api: &dyn GitHubApi,
) -> Result<J> {
    validation::dependencies(s, &batch_state.lane, &batch_state.batch, api)?;
    snapshot_latest(s, spec, now, batch_state, api)
}

fn snapshot_latest(
    s: &mut J,
    spec: &J,
    now: f64,
    batch_state: BatchState,
    api: &dyn GitHubApi,
) -> Result<J> {
    let latest = validation::latest(&batch_state.lane, api)?;
    snapshot_target(s, spec, now, batch_state, &latest)
}

fn snapshot_target(
    s: &mut J,
    spec: &J,
    now: f64,
    batch_state: BatchState,
    latest: &str,
) -> Result<J> {
    if latest != model::string(&batch_state.batch, "base").unwrap_or_default() {
        return Err(FlowError::progress(
            "target changed before snapshot freeze; rebase this frozen batch",
        ));
    }
    let bare = snapshot_bare(&batch_state.batch);
    snapshot_head(s, spec, now, batch_state, bare)
}

fn snapshot_bare(batch: &J) -> PathBuf {
    Path::new(&model::string(batch, "workspace").unwrap_or_default()).join("objects.git")
}

fn snapshot_head(
    s: &mut J,
    spec: &J,
    now: f64,
    batch_state: BatchState,
    bare: PathBuf,
) -> Result<J> {
    let head = snapshot_candidate_ref(&bare)?;
    snapshot_tree(s, spec, now, batch_state, bare, head)
}

fn snapshot_candidate_ref(bare: &Path) -> Result<String> {
    ledger::sha(Some(&json!(git::output(
        bare,
        &["rev-parse".to_string(), "refs/herdr/candidate".to_string()]
    )?)))
}

fn snapshot_tree(
    s: &mut J,
    spec: &J,
    now: f64,
    batch_state: BatchState,
    bare: PathBuf,
    head: String,
) -> Result<J> {
    let tree = snapshot_tree_ref(&bare, &head)?;
    let checkout = snapshot_checkout_path(&batch_state.batch);
    snapshot_checkout(
        s,
        spec,
        now,
        batch_state,
        CandidateSource {
            bare,
            checkout,
            head,
        },
        tree,
    )
}

fn snapshot_tree_ref(bare: &Path, head: &str) -> Result<String> {
    ledger::sha(Some(&json!(git::output(
        bare,
        &["rev-parse".to_string(), format!("{head}^{{tree}}")]
    )?)))
}

fn snapshot_checkout_path(batch: &J) -> PathBuf {
    Path::new(&model::string(batch, "workspace").unwrap_or_default()).join("snapshot")
}

fn snapshot_checkout(
    s: &mut J,
    spec: &J,
    now: f64,
    batch_state: BatchState,
    candidate_source: CandidateSource,
    tree: String,
) -> Result<J> {
    if candidate_source.checkout.exists() {
        snapshot_reuse(s, spec, now, batch_state, candidate_source, tree)
    } else {
        snapshot_create(s, spec, now, batch_state, candidate_source, tree)
    }
}

fn snapshot_reuse(
    s: &mut J,
    spec: &J,
    now: f64,
    batch_state: BatchState,
    candidate_source: CandidateSource,
    tree: String,
) -> Result<J> {
    let root = git::output(
        &candidate_source.checkout,
        &["rev-parse".to_string(), "--show-toplevel".to_string()],
    )?;
    snapshot_reuse_status(s, spec, now, batch_state, candidate_source, tree, root)
}

fn snapshot_reuse_status(
    s: &mut J,
    spec: &J,
    now: f64,
    batch_state: BatchState,
    candidate_source: CandidateSource,
    tree: String,
    root: String,
) -> Result<J> {
    let status = git::output(
        &candidate_source.checkout,
        &[
            "status".to_string(),
            "--porcelain".to_string(),
            "--untracked-files=all".to_string(),
        ],
    )?;
    snapshot_reuse_guard(
        s,
        spec,
        now,
        batch_state,
        candidate_source,
        tree,
        SnapshotStatus { root, status },
    )
}

fn snapshot_reuse_guard(
    s: &mut J,
    spec: &J,
    now: f64,
    batch_state: BatchState,
    candidate_source: CandidateSource,
    tree: String,
    snapshot_status: SnapshotStatus,
) -> Result<J> {
    if checkout_dirty(
        &snapshot_status.root,
        &candidate_source.checkout,
        &snapshot_status.status,
    ) {
        return Err(FlowError::progress(
            "snapshot checkout has unaccounted changes; inspect without reset",
        ));
    }
    snapshot_reuse_checkout(s, spec, now, batch_state, candidate_source, tree)
}

fn checkout_dirty(root: &str, checkout: &Path, status: &str) -> bool {
    root != checkout.display().to_string() || !status.is_empty()
}

fn snapshot_reuse_checkout(
    s: &mut J,
    spec: &J,
    now: f64,
    batch_state: BatchState,
    candidate_source: CandidateSource,
    tree: String,
) -> Result<J> {
    git::run(
        &candidate_source.checkout,
        &[
            "checkout".to_string(),
            "--detach".to_string(),
            candidate_source.head.clone(),
        ],
        None,
        None,
        &std::collections::BTreeMap::new(),
        false,
    )?;
    snapshot_candidate(s, spec, now, batch_state, candidate_source, tree)
}

fn snapshot_create(
    s: &mut J,
    spec: &J,
    now: f64,
    batch_state: BatchState,
    candidate_source: CandidateSource,
    tree: String,
) -> Result<J> {
    git::run(
        &candidate_source.bare,
        &[
            "worktree".to_string(),
            "add".to_string(),
            "--detach".to_string(),
            candidate_source.checkout.display().to_string(),
            candidate_source.head.clone(),
        ],
        None,
        None,
        &std::collections::BTreeMap::new(),
        false,
    )?;
    snapshot_candidate(s, spec, now, batch_state, candidate_source, tree)
}

fn snapshot_candidate(
    s: &mut J,
    spec: &J,
    now: f64,
    mut batch_state: BatchState,
    candidate_source: CandidateSource,
    tree: String,
) -> Result<J> {
    candidate(&mut batch_state.task, &candidate_source.head)?;
    let revision = model::int(&batch_state.batch, "revision").unwrap_or(0) + 1;
    batch_state.batch["revision"] = json!(revision);
    batch_state.batch["snapshot"] = json!({"head": candidate_source.head.clone(), "tree": tree.clone(), "frozen_at": now, "revision": revision});
    batch_state.batch["gate"] = Value::Null;
    batch_state.batch["phase"] = json!("snapshot_frozen");
    batch_state.batch["progress_deadline"] =
        json!(now + model::number(&batch_state.lane, "progress_timeout").unwrap_or(600.0));
    let prior_pr = model::get(&batch_state.batch, "previous_pr").cloned();
    model::remove(&mut batch_state.task, "pr");
    snapshot_own(s, spec, now, batch_state, candidate_source, tree, prior_pr)
}

fn snapshot_own(
    s: &mut J,
    spec: &J,
    now: f64,
    batch_state: BatchState,
    candidate_source: CandidateSource,
    _tree: String,
    prior_pr: Option<J>,
) -> Result<J> {
    let base = model::string(&batch_state.batch, "base").unwrap_or_default();
    let own = snapshot_own_commits(&candidate_source.bare, &base, &candidate_source.head)?;
    snapshot_prepare(
        s,
        spec,
        now,
        batch_state,
        candidate_source.checkout,
        candidate_source.head,
        SnapshotPublication {
            own_commits: own,
            previous_pr: prior_pr,
        },
    )
}

fn snapshot_own_commits(bare: &Path, base: &str, head: &str) -> Result<Vec<String>> {
    Ok(git::output(
        bare,
        &[
            "rev-list".to_string(),
            "--reverse".to_string(),
            format!("{base}..{head}"),
        ],
    )?
    .lines()
    .map(str::trim)
    .filter(|line| !line.is_empty())
    .map(ToString::to_string)
    .collect())
}

fn snapshot_prepare(
    s: &mut J,
    spec: &J,
    now: f64,
    batch_state: BatchState,
    checkout: PathBuf,
    head: String,
    snapshot_publication: SnapshotPublication,
) -> Result<J> {
    let BatchState {
        task: t,
        lane,
        batch,
    } = batch_state;

    let mut working = s.clone();
    let task_name = model::string(&t, "task").unwrap_or_default();
    working["tasks"][&task_name] = t.clone();
    let prepare_spec = json!({
        "task": task_name,
        "state_version": t.get("state_version").cloned().unwrap_or(Value::Null),
        "repo": lane.get("repo").cloned().unwrap_or(Value::Null),
        "host": lane.get("host").cloned().unwrap_or(Value::Null),
        "checkout": checkout.display().to_string(),
        "issue": spec.get("issue").cloned().unwrap_or(Value::Null),
        "original_base": batch.get("base").cloned().unwrap_or(Value::Null),
        "head": head,
        "own_commits": snapshot_publication.own_commits,
        "dependencies": [],
        "target_base": batch.get("base").cloned().unwrap_or(Value::Null),
        "base_ref": lane.get("base_ref").cloned().unwrap_or(Value::Null),
        "head_ref": format!("herdr/{}", model::string(&batch, "id").unwrap_or_default()),
        "max_repairs": lane.get("max_repairs").cloned().unwrap_or(Value::Null)
    });
    ledger::prepare(&mut working, &prepare_spec, now)?;
    let t = working["tasks"][&task_name].clone();
    snapshot_prior(
        s,
        spec,
        now,
        BatchState {
            task: t,
            lane,
            batch,
        },
        snapshot_publication.previous_pr,
    )
}

fn snapshot_prior(
    s: &mut J,
    spec: &J,
    now: f64,
    mut batch_state: BatchState,
    prior_pr: Option<J>,
) -> Result<J> {
    if let Some(prior) = prior_pr {
        snapshot_apply_prior(&mut batch_state.task, &prior, spec, now);
    }
    snapshot_finish(s, now, batch_state)
}

fn snapshot_apply_prior(t: &mut J, prior: &J, spec: &J, now: f64) {
    let versions = model::get(t, "pr")
        .and_then(|pr| model::array_field(pr, "versions"))
        .cloned()
        .unwrap_or_default();
    let mut updated_versions = versions;
    updated_versions.push(json!({
        "head": prior.get("head").cloned().unwrap_or(Value::Null),
        "own_commits": prior.get("own_commits").cloned().unwrap_or(Value::Null),
        "observation": prior.get("observation").cloned().unwrap_or(Value::Null)
    }));
    t["pr"]["number"] = prior.get("number").cloned().unwrap_or(Value::Null);
    t["pr"]["versions"] = json!(updated_versions);
    t["pr"]["publication"] = json!({
        "kind": "update",
        "phase": "prepared",
        "previous_head": prior.get("head").cloned().unwrap_or(Value::Null),
        "head": t["pr"]["head"].clone(),
        "prepared_at": now,
        "evidence": spec.get("evidence").cloned().unwrap_or_else(|| json!("batch snapshot journal"))
    });
}

fn snapshot_finish(s: &mut J, now: f64, mut batch_state: BatchState) -> Result<J> {
    block(
        &mut batch_state.task,
        "final snapshot frozen",
        "batch-gate-begin followed by trusted gate verification",
    );
    batch_state.lane["active"] = batch_state.batch;
    batch_state.task["lane"] = batch_state.lane;
    save(s, batch_state.task, now, "batch_final_snapshot_frozen")
}

pub fn gate_begin(s: &mut J, spec: &J, now: f64, api: Option<&dyn GitHubApi>) -> Result<J> {
    let (t, lane) = mutable(s, spec)?;
    gate_begin_idle(s, spec, now, api, t, lane)
}

fn gate_begin_idle(
    s: &mut J,
    spec: &J,
    now: f64,
    api: Option<&dyn GitHubApi>,
    t: J,
    lane: J,
) -> Result<J> {
    idle(&t)?;
    gate_begin_active(s, spec, now, api, t, lane)
}

fn gate_begin_active(
    s: &mut J,
    spec: &J,
    now: f64,
    api: Option<&dyn GitHubApi>,
    t: J,
    lane: J,
) -> Result<J> {
    let batch = active(&lane, spec)?;
    gate_begin_phase(
        s,
        spec,
        now,
        api,
        BatchState {
            task: t,
            lane,
            batch,
        },
    )
}

fn gate_begin_phase(
    s: &mut J,
    spec: &J,
    now: f64,
    api: Option<&dyn GitHubApi>,
    batch_state: BatchState,
) -> Result<J> {
    if gate_begin_ready(&batch_state.batch) {
        gate_begin_api(s, spec, now, api, batch_state)
    } else {
        Err(FlowError::progress(
            "gate may already have executed; inspect its existing operation instead of replaying",
        ))
    }
}

fn gate_begin_ready(batch: &J) -> bool {
    [
        model::string(batch, "phase").as_deref() == Some("snapshot_frozen"),
        !model::truthy(model::get(batch, "gate")),
    ]
    .into_iter()
    .all(|ok| ok)
}

fn gate_begin_api(
    s: &mut J,
    spec: &J,
    now: f64,
    api: Option<&dyn GitHubApi>,
    batch_state: BatchState,
) -> Result<J> {
    let owned;
    let api: &dyn GitHubApi = if let Some(api) = api {
        api
    } else {
        owned = GitHub::new(batch_state.lane.clone());
        &owned
    };
    gate_begin_deps(s, spec, now, batch_state, api)
}

fn gate_begin_deps(
    s: &mut J,
    spec: &J,
    now: f64,
    batch_state: BatchState,
    api: &dyn GitHubApi,
) -> Result<J> {
    validation::dependencies(s, &batch_state.lane, &batch_state.batch, api)?;
    gate_begin_latest(s, spec, now, batch_state, api)
}

fn gate_begin_latest(
    s: &mut J,
    spec: &J,
    now: f64,
    batch_state: BatchState,
    api: &dyn GitHubApi,
) -> Result<J> {
    let latest = validation::latest(&batch_state.lane, api)?;
    gate_begin_target(s, spec, now, batch_state, &latest)
}

fn gate_begin_target(
    s: &mut J,
    spec: &J,
    now: f64,
    batch_state: BatchState,
    latest: &str,
) -> Result<J> {
    if latest != model::string(&batch_state.batch, "base").unwrap_or_default() {
        return Err(FlowError::progress(
            "target changed; old snapshot cannot start final validation",
        ));
    }
    gate_begin_wait(s, spec, now, batch_state)
}

fn gate_begin_wait(s: &mut J, spec: &J, now: f64, batch_state: BatchState) -> Result<J> {
    let wait = progress::wait_for(s, model::get(spec, "wait"), &batch_state.task)?;
    gate_begin_wait_check(s, spec, now, batch_state, wait)
}

fn gate_begin_wait_check(
    s: &mut J,
    spec: &J,
    now: f64,
    batch_state: BatchState,
    wait: J,
) -> Result<J> {
    if gate_wait_ok(s, &wait, now) {
        gate_begin_operation(s, spec, now, batch_state, wait)
    } else {
        Err(FlowError::progress(
            "gate execution requires a fresh pinned result wait with an unexpired deadline",
        ))
    }
}

fn gate_wait_ok(s: &J, wait: &J, now: f64) -> bool {
    [
        model::string(wait, "kind").as_deref() == Some("signal"),
        model::truthy(model::get(wait, "producer_session")),
        gate_wait_deadline(wait, now),
        gate_wait_unconsumed(s, wait),
    ]
    .into_iter()
    .all(|ok| ok)
}

fn gate_wait_deadline(wait: &J, now: f64) -> bool {
    model::number(wait, "deadline").unwrap_or(0.0) > now
}

fn gate_wait_unconsumed(s: &J, wait: &J) -> bool {
    !model::get(s, "events")
        .and_then(Value::as_object)
        .map(|events| {
            events
                .values()
                .any(|event| model::string(event, "wait") == model::string(wait, "id"))
        })
        .unwrap_or(false)
}

fn gate_begin_operation(
    s: &mut J,
    spec: &J,
    now: f64,
    batch_state: BatchState,
    wait: J,
) -> Result<J> {
    let operation = progress::text(model::get(spec, "operation"), "gate operation", 128)?;
    gate_begin_duplicate(s, now, batch_state, wait, operation)
}

fn gate_begin_duplicate(
    s: &mut J,
    now: f64,
    batch_state: BatchState,
    wait: J,
    operation: String,
) -> Result<J> {
    if gate_operation_duplicate(s, &operation) {
        return Err(FlowError::progress(
            "gate operation ID already used; recover it rather than replaying",
        ));
    }
    gate_begin_count(s, now, batch_state, wait, operation)
}

fn gate_operation_duplicate(s: &J, operation: &str) -> bool {
    model::get(s, "tasks")
        .and_then(Value::as_object)
        .map(|tasks| {
            tasks
                .values()
                .any(|task| model::dig(task, &["lane", "gate_operations", operation]).is_some())
        })
        .unwrap_or(false)
}

fn gate_begin_count(
    s: &mut J,
    now: f64,
    batch_state: BatchState,
    wait: J,
    operation: String,
) -> Result<J> {
    let operation_count = gate_operation_count(&batch_state.lane);
    if operation_count >= 256 {
        return Err(FlowError::progress(
            "lane reached 256 gate operations; preserve unresolved state and use a new bounded contract",
        ));
    }
    gate_begin_bind(s, now, batch_state, wait, operation)
}

fn gate_operation_count(lane: &J) -> usize {
    model::get(lane, "gate_operations")
        .and_then(Value::as_object)
        .map(|items| items.len())
        .unwrap_or(0)
}

fn gate_begin_bind(
    s: &mut J,
    now: f64,
    mut batch_state: BatchState,
    wait: J,
    operation: String,
) -> Result<J> {
    batch_state.lane["gate_operations"][&operation] =
        gate::binding(&batch_state.lane, &batch_state.batch)?;
    gate_begin_state(s, now, batch_state, wait, operation)
}

fn gate_begin_state(
    s: &mut J,
    now: f64,
    mut batch_state: BatchState,
    wait: J,
    operation: String,
) -> Result<J> {
    batch_state.batch["gate"] = json!({
        "operation": operation,
        "phase": "sending",
        "started_at": now,
        "receipt": null
    });
    batch_state.batch["phase"] = json!("gate_uncertain");
    batch_state.batch["progress_deadline"] =
        json!(now + model::number(&batch_state.lane, "progress_timeout").unwrap_or(600.0));
    let wait_id = model::string(&wait, "id").unwrap_or_default();
    let wait_owner = model::string(&wait, "owner").unwrap_or_default();
    let wait_condition = model::string(&wait, "condition").unwrap_or_default();
    batch_state.task["phase"] = json!("waiting");
    batch_state.task["wait"] = json!(wait_id);
    batch_state.task["owner"] = json!(wait_owner);
    batch_state.task["next_action"] = json!({
        "kind": "wait",
        "wait": wait_id,
        "owner": wait_owner,
        "condition": wait_condition
    });
    batch_state.lane["active"] = batch_state.batch;
    batch_state.task["lane"] = batch_state.lane;
    save(s, batch_state.task, now, "batch_gate_started")
}

pub fn gate_record(s: &mut J, spec: &J, now: f64, api: Option<&dyn GitHubApi>) -> Result<J> {
    let (t, lane) = mutable(s, spec)?;
    gate_record_idle(s, spec, now, api, t, lane)
}

fn gate_record_idle(
    s: &mut J,
    spec: &J,
    now: f64,
    api: Option<&dyn GitHubApi>,
    t: J,
    lane: J,
) -> Result<J> {
    idle(&t)?;
    gate_record_active(s, spec, now, api, t, lane)
}

fn gate_record_active(
    s: &mut J,
    spec: &J,
    now: f64,
    api: Option<&dyn GitHubApi>,
    t: J,
    lane: J,
) -> Result<J> {
    let batch = active(&lane, spec)?;
    gate_record_phase(
        s,
        spec,
        now,
        api,
        BatchState {
            task: t,
            lane,
            batch,
        },
    )
}

fn gate_record_phase(
    s: &mut J,
    spec: &J,
    now: f64,
    api: Option<&dyn GitHubApi>,
    batch_state: BatchState,
) -> Result<J> {
    if gate_record_ready(&batch_state.batch) {
        gate_record_api(s, spec, now, api, batch_state)
    } else {
        Err(FlowError::progress(
            "batch has no existing gate operation to reconcile",
        ))
    }
}

fn gate_record_ready(batch: &J) -> bool {
    [
        gate_record_phase_ok(batch),
        model::truthy(model::get(batch, "gate")),
    ]
    .into_iter()
    .all(|ok| ok)
}

fn gate_record_phase_ok(batch: &J) -> bool {
    matches!(
        model::string(batch, "phase").as_deref(),
        Some("gate_uncertain" | "gate_passed" | "gate_failed")
    )
}

fn gate_record_api(
    s: &mut J,
    spec: &J,
    now: f64,
    api: Option<&dyn GitHubApi>,
    batch_state: BatchState,
) -> Result<J> {
    let owned;
    let api: &dyn GitHubApi = if let Some(api) = api {
        api
    } else {
        owned = GitHub::new(batch_state.lane.clone());
        &owned
    };
    gate_record_deps(s, spec, now, batch_state, api)
}

fn gate_record_deps(
    s: &mut J,
    spec: &J,
    now: f64,
    batch_state: BatchState,
    api: &dyn GitHubApi,
) -> Result<J> {
    validation::dependencies(s, &batch_state.lane, &batch_state.batch, api)?;
    gate_record_latest(s, spec, now, batch_state, api)
}

fn gate_record_latest(
    s: &mut J,
    spec: &J,
    now: f64,
    batch_state: BatchState,
    api: &dyn GitHubApi,
) -> Result<J> {
    let latest = validation::latest(&batch_state.lane, api)?;
    gate_record_target(s, spec, now, batch_state, &latest)
}

fn gate_record_target(
    s: &mut J,
    spec: &J,
    now: f64,
    batch_state: BatchState,
    latest: &str,
) -> Result<J> {
    if latest != model::string(&batch_state.batch, "base").unwrap_or_default() {
        return Err(FlowError::progress(
            "target changed; refuse results for the old snapshot",
        ));
    }
    gate_record_query(s, spec, now, batch_state)
}

fn gate_record_query(s: &mut J, spec: &J, now: f64, batch_state: BatchState) -> Result<J> {
    let reference = model::get(spec, "reference").cloned();
    let receipt = gate::query(
        &batch_state.lane,
        &batch_state.batch,
        reference.as_ref(),
        now,
    )?;
    gate_record_identity(s, now, batch_state, receipt, reference)
}

fn gate_record_identity(
    s: &mut J,
    now: f64,
    batch_state: BatchState,
    receipt: J,
    reference: Option<J>,
) -> Result<J> {
    if gate_receipt_identity_changed(&batch_state.batch, &receipt) {
        return Err(FlowError::progress(
            "gate run identity changed; inspect instead of replacing a receipt",
        ));
    }
    gate_record_apply(s, now, batch_state, receipt, reference)
}

fn gate_receipt_identity_changed(batch: &J, receipt: &J) -> bool {
    model::dig(batch, &["gate", "receipt"])
        .cloned()
        .filter(|value| !value.is_null())
        .map(|previous| gate_receipt_run_changed(&previous, receipt))
        .unwrap_or(false)
}

fn gate_receipt_run_changed(previous: &J, receipt: &J) -> bool {
    [
        model::string(previous, "run") != model::string(receipt, "run"),
        model::string(previous, "external_id") != model::string(receipt, "external_id"),
    ]
    .into_iter()
    .any(|changed| changed)
}

fn gate_record_apply(
    s: &mut J,
    now: f64,
    mut batch_state: BatchState,
    receipt: J,
    reference: Option<J>,
) -> Result<J> {
    batch_state.batch["gate"]["phase"] = json!("observed");
    batch_state.batch["gate"]["reference"] = reference.unwrap_or(Value::Null);
    batch_state.batch["gate"]["receipt"] = receipt.clone();
    let decision = model::string(&receipt, "decision").unwrap_or_default();
    batch_state.batch["phase"] = json!(gate_phase_label(&decision));
    batch_state.batch["failure_token"] = gate_failure_token(&batch_state.batch, &decision);
    block(
        &mut batch_state.task,
        &format!("trusted gate {decision}"),
        gate_condition(&decision),
    );
    batch_state.lane["active"] = batch_state.batch;
    batch_state.task["lane"] = batch_state.lane;
    save(s, batch_state.task, now, "batch_gate_result_verified")
}

fn gate_phase_label(decision: &str) -> &'static str {
    if decision == "allow" {
        "gate_passed"
    } else {
        "gate_failed"
    }
}

fn gate_failure_token(batch: &J, decision: &str) -> J {
    if decision == "deny" {
        json!(repair_token(batch))
    } else {
        Value::Null
    }
}

fn gate_condition(decision: &str) -> &'static str {
    if decision == "allow" {
        "authorized publication"
    } else {
        "inspect code diagnosis and apply one bounded V1 repair assignment"
    }
}

pub fn reconcile(s: &mut J, spec: &J, now: f64, api: Option<&dyn GitHubApi>) -> Result<J> {
    let (t, lane) = mutable(s, spec)?;
    reconcile_active(s, spec, now, api, t, lane)
}

fn reconcile_active(
    s: &mut J,
    spec: &J,
    now: f64,
    api: Option<&dyn GitHubApi>,
    t: J,
    lane: J,
) -> Result<J> {
    let _ = active(&lane, spec)?;
    reconcile_idle(s, spec, now, api, t, lane)
}

fn reconcile_idle(
    s: &mut J,
    spec: &J,
    now: f64,
    api: Option<&dyn GitHubApi>,
    t: J,
    lane: J,
) -> Result<J> {
    idle(&t)?;
    reconcile_api(s, spec, now, api, lane)
}

#[derive(Clone, Copy)]
struct ReconcileCtx<'a> {
    now: f64,
    api: &'a dyn GitHubApi,
}

fn reconcile_api(s: &mut J, spec: &J, now: f64, api: Option<&dyn GitHubApi>, lane: J) -> Result<J> {
    let owned;
    let api: &dyn GitHubApi = if let Some(api) = api {
        api
    } else {
        owned = GitHub::new(lane.clone());
        &owned
    };
    reconcile_run(s, spec, now, api)
}

fn reconcile_run(s: &mut J, spec: &J, now: f64, api: &dyn GitHubApi) -> Result<J> {
    let mut working = s.clone();
    let t = crate::pr::reconcile(&mut working, spec, now, Some(api))?;
    let ctx = ReconcileCtx { now, api };
    reconcile_state(&mut working, s, t, ctx)
}

fn reconcile_state(working: &mut J, s: &mut J, t: J, ctx: ReconcileCtx<'_>) -> Result<J> {
    let lane = reconcile_lane(&t);
    let batch = reconcile_batch(&lane)?;
    reconcile_pr(
        working,
        s,
        BatchState {
            task: t,
            lane,
            batch,
        },
        ctx,
    )
}

fn reconcile_lane(t: &J) -> J {
    model::get(t, "lane").cloned().unwrap_or_else(|| json!({}))
}

fn reconcile_batch(lane: &J) -> Result<J> {
    model::get(lane, "active")
        .filter(|value| model::truthy(Some(value)))
        .cloned()
        .ok_or_else(|| FlowError::progress("exact active batch ID required"))
}

fn reconcile_pr(
    working: &mut J,
    s: &mut J,
    batch_state: BatchState,
    ctx: ReconcileCtx<'_>,
) -> Result<J> {
    let r = reconcile_pr_value(&batch_state.task)?;
    reconcile_phase(working, s, batch_state, r, ctx)
}

fn reconcile_pr_value(t: &J) -> Result<J> {
    model::get(t, "pr")
        .cloned()
        .ok_or_else(|| FlowError::progress("PR provenance is unavailable"))
}

fn reconcile_phase(
    working: &mut J,
    s: &mut J,
    mut batch_state: BatchState,
    r: J,
    ctx: ReconcileCtx<'_>,
) -> Result<J> {
    if model::string(&r, "phase").as_deref() != Some("merged") {
        block(
            &mut batch_state.task,
            &format!(
                "batch PR {}",
                model::string(&r, "phase").unwrap_or_default()
            ),
            "observe the existing PR and its exact trusted Check",
        );
        s["tasks"][&model::string(&batch_state.task, "task").unwrap_or_default()] =
            batch_state.task.clone();
        return Ok(batch_state.task);
    }
    reconcile_merged(working, s, batch_state, r, ctx)
}

fn reconcile_merged(
    working: &mut J,
    s: &mut J,
    batch_state: BatchState,
    r: J,
    ctx: ReconcileCtx<'_>,
) -> Result<J> {
    validation::dependencies(s, &batch_state.lane, &batch_state.batch, ctx.api)?;
    reconcile_landing(working, s, batch_state, r, ctx)
}

fn reconcile_landing(
    working: &mut J,
    s: &mut J,
    batch_state: BatchState,
    r: J,
    ctx: ReconcileCtx<'_>,
) -> Result<J> {
    let landing = model::get(&r, "landing")
        .cloned()
        .ok_or_else(|| FlowError::progress("PR landing is unavailable"))?;
    reconcile_receipt(
        working,
        s,
        batch_state,
        BatchLanding {
            pull_request: r,
            landing,
        },
        ctx,
    )
}

fn reconcile_receipt(
    working: &mut J,
    s: &mut J,
    batch_state: BatchState,
    batch_landing: BatchLanding,
    ctx: ReconcileCtx<'_>,
) -> Result<J> {
    let receipt = reconcile_receipt_value(&batch_state.batch)?;
    reconcile_receipt_decision(working, s, batch_state, batch_landing, receipt, ctx)
}

fn reconcile_receipt_value(batch: &J) -> Result<J> {
    model::dig(batch, &["gate", "receipt"])
        .cloned()
        .filter(|value| model::truthy(Some(value)))
        .ok_or_else(|| {
            FlowError::progress(
                "merged batch has no complete verified trusted gate result; retain the slot",
            )
        })
}

fn reconcile_receipt_decision(
    working: &mut J,
    s: &mut J,
    batch_state: BatchState,
    batch_landing: BatchLanding,
    receipt: J,
    ctx: ReconcileCtx<'_>,
) -> Result<J> {
    if model::string(&receipt, "decision").as_deref() != Some("allow") {
        return Err(FlowError::progress(
            "merged batch has no complete verified trusted gate result; retain the slot",
        ));
    }
    reconcile_checks(working, s, batch_state, batch_landing, receipt, ctx)
}

fn reconcile_checks(
    working: &mut J,
    s: &mut J,
    batch_state: BatchState,
    batch_landing: BatchLanding,
    receipt: J,
    ctx: ReconcileCtx<'_>,
) -> Result<J> {
    let config = reconcile_check_config(&batch_state.batch, &batch_state.lane, &receipt);
    let checks = crate::ci::checks::run_rows(&config, ctx.api)?;
    reconcile_check_state(working, s, batch_state, batch_landing, checks, ctx)
}

fn reconcile_check_config(batch: &J, lane: &J, receipt: &J) -> J {
    json!({
        "sha": batch.get("snapshot").and_then(|snapshot| snapshot.get("head")).cloned().unwrap_or(Value::Null),
        "checks": [{
            "name": lane.get("gate").and_then(|gate| gate.get("check_name")).cloned().unwrap_or(Value::Null),
            "app_id": lane.get("gate").and_then(|gate| gate.get("app_id")).cloned().unwrap_or(Value::Null),
            "external_id": receipt.get("external_id").cloned().unwrap_or(Value::Null)
        }]
    })
}

fn reconcile_check_state(
    working: &mut J,
    s: &mut J,
    batch_state: BatchState,
    batch_landing: BatchLanding,
    checks: Vec<J>,
    ctx: ReconcileCtx<'_>,
) -> Result<J> {
    if !reconcile_checks_ok(&checks) {
        return Err(FlowError::progress(
            "merged batch trusted Check is missing, superseded or revoked; retain the slot",
        ));
    }
    reconcile_commit(working, s, batch_state, batch_landing, ctx)
}

fn reconcile_checks_ok(checks: &[J]) -> bool {
    [
        checks.len() == 1,
        checks
            .first()
            .and_then(|check| model::string(check, "state"))
            .as_deref()
            == Some("success"),
    ]
    .into_iter()
    .all(|ok| ok)
}

fn reconcile_commit(
    working: &mut J,
    s: &mut J,
    batch_state: BatchState,
    batch_landing: BatchLanding,
    ctx: ReconcileCtx<'_>,
) -> Result<J> {
    let commit = ctx.api.get(&format!(
        "git/commits/{}",
        model::string(&batch_landing.landing, "commit").unwrap_or_default()
    ))?;
    reconcile_commit_tree(working, s, batch_state, batch_landing, commit, ctx)
}

fn reconcile_commit_tree(
    working: &mut J,
    s: &mut J,
    batch_state: BatchState,
    batch_landing: BatchLanding,
    commit: J,
    ctx: ReconcileCtx<'_>,
) -> Result<J> {
    let actual = ledger::sha(model::get(&commit, "tree").and_then(|tree| tree.get("sha")))?;
    reconcile_commit_compare(working, s, batch_state, batch_landing, actual, ctx)
}

fn reconcile_commit_compare(
    working: &mut J,
    s: &mut J,
    batch_state: BatchState,
    batch_landing: BatchLanding,
    actual: String,
    ctx: ReconcileCtx<'_>,
) -> Result<J> {
    let expected = ledger::sha(model::dig(&batch_state.batch, &["snapshot", "tree"]))?;
    reconcile_commit_guard(
        working,
        s,
        batch_state,
        batch_landing,
        actual,
        expected,
        ctx,
    )
}

fn reconcile_commit_guard(
    working: &mut J,
    s: &mut J,
    batch_state: BatchState,
    batch_landing: BatchLanding,
    actual: String,
    expected: String,
    ctx: ReconcileCtx<'_>,
) -> Result<J> {
    if actual != expected {
        return Err(FlowError::progress(
            "merged content differs from the validated final snapshot; retain the slot",
        ));
    }
    reconcile_members(working, s, batch_state, batch_landing, ctx)
}

fn reconcile_members(
    working: &mut J,
    s: &mut J,
    batch_state: BatchState,
    batch_landing: BatchLanding,
    ctx: ReconcileCtx<'_>,
) -> Result<J> {
    let members = model::array_field(&batch_state.batch, "members")
        .cloned()
        .unwrap_or_default();
    members.iter().try_for_each(|member| {
        reconcile_member(
            working,
            member,
            &batch_state.batch,
            &batch_landing.landing,
            ctx,
        )
    })?;
    reconcile_land(working, s, batch_state, batch_landing, ctx)
}

fn reconcile_member(
    working: &mut J,
    member: &J,
    batch: &J,
    landing: &J,
    ctx: ReconcileCtx<'_>,
) -> Result<()> {
    let task_name = model::string(member, "task").unwrap_or_default();
    let mut mt = model::dig(working, &["tasks", &task_name])
        .cloned()
        .unwrap_or_else(|| json!({}));
    let mut mapping = landing.clone();
    mapping["head"] = member.get("head").cloned().unwrap_or(Value::Null);
    mapping["batch_head"] = batch
        .get("snapshot")
        .and_then(|snapshot| snapshot.get("head"))
        .cloned()
        .unwrap_or(Value::Null);
    mapping["tree"] = batch
        .get("snapshot")
        .and_then(|snapshot| snapshot.get("tree"))
        .cloned()
        .unwrap_or(Value::Null);
    mapping["batch"] = batch.get("id").cloned().unwrap_or(Value::Null);
    mt["batch_delivery"]["phase"] = json!("landed");
    mt["batch_delivery"]["landing"] = mapping;
    mt["batch_delivery"]["acceptance"] = member.get("acceptance").cloned().unwrap_or(Value::Null);
    ledger::store(working, mt, ctx.now, "batch_member_landing_confirmed")?;
    Ok(())
}

fn reconcile_land(
    working: &mut J,
    s: &mut J,
    mut batch_state: BatchState,
    batch_landing: BatchLanding,
    ctx: ReconcileCtx<'_>,
) -> Result<J> {
    batch_state.batch["phase"] = json!("landed");
    batch_state.batch["landing"] = batch_landing.landing;
    batch_state.batch["pr"] = batch_landing.pull_request;
    batch_state.batch["landed_at"] = json!(ctx.now);
    push_history(&mut batch_state.lane, &batch_state.batch);
    batch_state.lane["active"] = Value::Null;
    model::remove(&mut batch_state.task, "pr");
    batch_state.task["lane"] = batch_state.lane.clone();
    if lane_has_pending(&batch_state.lane) {
        reconcile_freeze(working, s, batch_state.task, batch_state.lane, ctx)
    } else {
        reconcile_idle_landing(working, s, batch_state.task, ctx)
    }
}

fn push_history(lane: &mut J, batch: &J) {
    if model::get(lane, "history")
        .and_then(Value::as_array)
        .is_some()
    {
        push_history_entries(lane, batch);
    }
}

fn push_history_entries(lane: &mut J, batch: &J) {
    let mut history = model::array_field(lane, "history")
        .cloned()
        .unwrap_or_default();
    history.push(batch.clone());
    let (history, overflow) = trim_history(history);
    lane["history"] = json!(history);
    if overflow {
        increment_older_batches(lane);
    }
}

fn trim_history(mut history: Vec<J>) -> (Vec<J>, bool) {
    if history.len() > 32 {
        let keep = history.split_off(history.len() - 32);
        (keep, true)
    } else {
        (history, false)
    }
}

fn increment_older_batches(lane: &mut J) {
    let older = model::int(lane, "older_batches").unwrap_or(0) + 1;
    lane["older_batches"] = json!(older);
}

fn lane_has_pending(lane: &J) -> bool {
    model::array_field(lane, "pending")
        .map(|items| !items.is_empty())
        .unwrap_or(false)
}

fn reconcile_freeze(working: &mut J, s: &mut J, t: J, lane: J, ctx: ReconcileCtx<'_>) -> Result<J> {
    let freeze_t = freeze_next(working, t, lane, ctx.now, ctx.api)?;
    let task_name = model::string(&freeze_t, "task").unwrap_or_default();
    working["tasks"][&task_name] = freeze_t.clone();
    s["tasks"] = working["tasks"].clone();
    Ok(freeze_t)
}

fn reconcile_idle_landing(
    working: &mut J,
    s: &mut J,
    mut t: J,
    ctx: ReconcileCtx<'_>,
) -> Result<J> {
    block(
        &mut t,
        "no pending completed tasks",
        "batch-offer; no work or gate run is manufactured",
    );
    let task_name = model::string(&t, "task").unwrap_or_default();
    working["tasks"][&task_name] = t.clone();
    s["tasks"] = working["tasks"].clone();
    save(s, t, ctx.now, "batch_landed_and_successor_prepared")
}

pub fn repair_token(batch: &J) -> String {
    sha256_json_sorted(&json!({
        "batch": batch.get("id").cloned().unwrap_or(Value::Null),
        "revision": batch.get("revision").cloned().unwrap_or(Value::Null),
        "phase": batch.get("phase").cloned().unwrap_or(Value::Null),
        "conflict": batch.get("integration").and_then(|integration| integration.get("conflict")).cloned().unwrap_or(Value::Null),
        "receipt": batch.get("gate").and_then(|gate| gate.get("receipt")).cloned().unwrap_or(Value::Null)
    }))
}

pub fn repair_intent(t: &mut J, action: &J, event: &J, now: f64) -> Result<()> {
    let Some(lane) = repair_lane(t, action)? else {
        return Ok(());
    };
    repair_intent_batch(t, action, event, now, lane)
}

fn repair_lane(t: &J, action: &J) -> Result<Option<J>> {
    let lane = model::get(t, "lane")
        .filter(|value| model::truthy(Some(value)))
        .cloned();
    repair_missing_lane(lane, action)
}

fn repair_missing_lane(lane: Option<J>, action: &J) -> Result<Option<J>> {
    if repair_lane_missing(&lane, action) {
        return Err(FlowError::progress("batch repair has no active lane"));
    }
    Ok(lane)
}

fn repair_lane_missing(lane: &Option<J>, action: &J) -> bool {
    [lane.is_none(), model::get(action, "batch_repair").is_some()]
        .into_iter()
        .all(|missing| missing)
}

fn repair_intent_batch(t: &mut J, action: &J, event: &J, now: f64, lane: J) -> Result<()> {
    let Some(batch) = repair_active_batch(&lane, action)? else {
        return Ok(());
    };
    repair_intent_writes(t, action, event, now, lane, batch)
}

fn repair_active_batch(lane: &J, action: &J) -> Result<Option<J>> {
    let batch = model::get(lane, "active")
        .filter(|value| model::truthy(Some(value)))
        .cloned();
    repair_missing_batch(batch, action)
}

fn repair_missing_batch(batch: Option<J>, action: &J) -> Result<Option<J>> {
    if repair_batch_missing(&batch, action) {
        return Err(FlowError::progress("batch repair has no active lane"));
    }
    Ok(batch)
}

fn repair_batch_missing(batch: &Option<J>, action: &J) -> bool {
    [
        batch.is_none(),
        model::get(action, "batch_repair").is_some(),
    ]
    .into_iter()
    .all(|missing| missing)
}

fn repair_intent_writes(
    t: &mut J,
    action: &J,
    event: &J,
    now: f64,
    lane: J,
    batch: J,
) -> Result<()> {
    let writes = action_writes(action)?;
    repair_intent_choose(writes, t, action, event, now, lane, batch)
}

fn repair_intent_choose(
    writes: bool,
    t: &mut J,
    action: &J,
    event: &J,
    now: f64,
    lane: J,
    batch: J,
) -> Result<()> {
    if writes {
        repair_intent_phase(t, action, event, now, lane, batch)
    } else {
        repair_refuse_unreserved(action)
    }
}

fn action_writes(action: &J) -> Result<bool> {
    model::get(action, "writes")
        .map(write_flag)
        .unwrap_or(Ok(true))
}

fn write_flag(value: &J) -> Result<bool> {
    value
        .as_bool()
        .ok_or_else(|| FlowError::progress("writes must be a boolean"))
}

fn repair_refuse_unreserved(action: &J) -> Result<()> {
    if model::get(action, "batch_repair").is_some() {
        Err(FlowError::progress("batch repair must reserve its writer"))
    } else {
        Ok(())
    }
}

fn repair_intent_phase(
    t: &mut J,
    action: &J,
    event: &J,
    now: f64,
    lane: J,
    batch: J,
) -> Result<()> {
    if !repair_phase_eligible(&batch) {
        return Err(FlowError::progress(
            "batch has no inspected failure eligible for repair",
        ));
    }
    let expected = repair_expected(t, &batch);
    repair_intent_binding(t, action, event, now, lane, batch, expected)
}

fn repair_phase_eligible(batch: &J) -> bool {
    matches!(
        model::string(batch, "phase").as_deref(),
        Some("integration_conflict" | "gate_failed")
    )
}

fn repair_expected(t: &J, batch: &J) -> J {
    json!({
        "batch": batch.get("id").cloned().unwrap_or(Value::Null),
        "token": repair_token(batch),
        "candidate": t.get("candidate").cloned().unwrap_or(Value::Null),
        "base": batch.get("base").cloned().unwrap_or(Value::Null)
    })
}

fn repair_intent_binding(
    t: &mut J,
    action: &J,
    event: &J,
    now: f64,
    lane: J,
    batch: J,
    expected: J,
) -> Result<()> {
    if !repair_binding_ok(action, &expected, event) {
        return Err(FlowError::progress(
            "writing batch dispatch requires the exact current failure token",
        ));
    }
    repair_intent_diagnosis(t, action, event, now, lane, batch, expected)
}

fn repair_binding_ok(action: &J, expected: &J, event: &J) -> bool {
    [
        model::get(action, "batch_repair") == Some(expected),
        repair_event_outcome_ok(event),
    ]
    .into_iter()
    .all(|ok| ok)
}

fn repair_event_outcome_ok(event: &J) -> bool {
    matches!(
        model::string(event, "outcome").as_deref(),
        Some("failed" | "decision")
    )
}

fn repair_intent_diagnosis(
    t: &mut J,
    action: &J,
    _event: &J,
    now: f64,
    lane: J,
    batch: J,
    expected: J,
) -> Result<()> {
    let diagnosis = progress::object_owned(
        model::get(action, "repair_diagnosis"),
        "batch repair diagnosis",
    )?;
    repair_intent_kind(t, action, now, lane, batch, expected, diagnosis)
}

fn repair_intent_kind(
    t: &mut J,
    action: &J,
    now: f64,
    lane: J,
    batch: J,
    expected: J,
    diagnosis: J,
) -> Result<()> {
    if model::string(&diagnosis, "kind").as_deref() != Some("code") {
        return Err(FlowError::progress(
            "only an inspected code conflict/failure can dispatch a batch repair",
        ));
    }
    repair_intent_evidence(t, action, now, lane, batch, expected, diagnosis)
}

fn repair_intent_evidence(
    t: &mut J,
    action: &J,
    now: f64,
    lane: J,
    batch: J,
    expected: J,
    diagnosis: J,
) -> Result<()> {
    progress::text(
        model::get(&diagnosis, "evidence"),
        "batch code diagnosis evidence",
        1000,
    )?;
    repair_intent_limit(t, action, now, lane, batch, expected)
}

fn repair_intent_limit(
    t: &mut J,
    action: &J,
    now: f64,
    lane: J,
    batch: J,
    expected: J,
) -> Result<()> {
    let token = model::string(&expected, "token").unwrap_or_default();
    let repairs = model::get(&lane, "repairs")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let same_batch = repairs
        .values()
        .filter(|repair| model::string(repair, "batch") == model::string(&batch, "id"))
        .count();
    if repair_limit_hit(&repairs, &token, same_batch, &lane) {
        return Err(FlowError::progress(
            "batch repair already dispatched or attempt limit reached",
        ));
    }
    repair_intent_workspace(t, action, now, lane, batch, expected, token)
}

fn repair_limit_hit(
    repairs: &serde_json::Map<String, J>,
    token: &str,
    same_batch: usize,
    lane: &J,
) -> bool {
    repairs.contains_key(token)
        || same_batch >= model::int(lane, "max_repairs").unwrap_or(0) as usize
}

fn repair_intent_workspace(
    t: &mut J,
    action: &J,
    now: f64,
    lane: J,
    batch: J,
    expected: J,
    token: String,
) -> Result<()> {
    let workspace = util::resolve_path(Path::new(&progress::text(
        model::get(action, "workspace"),
        "batch repair workspace",
        1000,
    )?));
    repair_intent_isolation(
        t,
        action,
        now,
        lane,
        batch,
        RepairBinding { expected, token },
        workspace,
    )
}

fn repair_intent_isolation(
    t: &mut J,
    action: &J,
    now: f64,
    lane: J,
    batch: J,
    repair_binding: RepairBinding,
    workspace: PathBuf,
) -> Result<()> {
    let lane_root_text = model::string(&lane, "workspace_root").unwrap_or_default();
    let lane_root = Path::new(&lane_root_text);
    let member_checkout = repair_member_checkout(&batch, &workspace);
    if repair_workspace_conflict(&workspace, lane_root, member_checkout) {
        return Err(FlowError::progress(
            "batch repair must use an isolated checkout",
        ));
    }
    repair_intent_store(
        t,
        action,
        now,
        lane,
        batch,
        repair_binding.expected,
        repair_binding.token,
    )
}

fn repair_workspace_conflict(workspace: &Path, lane_root: &Path, member_checkout: bool) -> bool {
    [util::overlap(workspace, lane_root), member_checkout]
        .into_iter()
        .any(|conflict| conflict)
}

fn repair_member_checkout(batch: &J, workspace: &Path) -> bool {
    model::array_field(batch, "members")
        .map(|members| {
            members.iter().any(|member| {
                model::string(member, "checkout")
                    .map(|checkout| util::overlap(workspace, Path::new(&checkout)))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

fn repair_intent_store(
    t: &mut J,
    action: &J,
    now: f64,
    mut lane: J,
    batch: J,
    expected: J,
    token: String,
) -> Result<()> {
    lane["repairs"][&token] = json!({
        "batch": batch.get("id").cloned().unwrap_or(Value::Null),
        "operation": action.get("operation").cloned().unwrap_or(Value::Null),
        "binding": expected,
        "phase": batch.get("phase").cloned().unwrap_or(Value::Null),
        "at": now
    });
    t["lane"] = lane;
    Ok(())
}

pub fn check_repair(s: &J, t: &J, op: &J, now: f64, live: bool) -> Result<()> {
    let lane = model::get(t, "lane").filter(|value| model::truthy(Some(value)));
    let intent = model::get(op, "intent")
        .cloned()
        .unwrap_or_else(|| json!({}));
    if repair_skipped(lane, &intent) {
        return Ok(());
    }
    let lane = lane.unwrap_or(&Value::Null);
    check_repair_batch(s, t, lane, &intent, now, live)
}

fn repair_skipped(lane: Option<&J>, intent: &J) -> bool {
    lane.is_none() || !model::bool_or(intent, "writes", true)
}

fn check_repair_batch(s: &J, t: &J, lane: &J, intent: &J, now: f64, live: bool) -> Result<()> {
    let batch = model::get(lane, "active")
        .filter(|value| model::truthy(Some(value)))
        .cloned();
    let binding = model::get(intent, "batch_repair").cloned();
    if !repair_binding_valid(&batch, &binding, t) {
        return Err(FlowError::progress(
            "batch changed before repair takeover; inspect the existing operation",
        ));
    }
    check_repair_live(s, now, live, lane, batch)
}

fn repair_binding_valid(batch: &Option<J>, binding: &Option<J>, t: &J) -> bool {
    batch
        .as_ref()
        .zip(binding.as_ref())
        .map(|(batch, binding)| repair_binding_matches(batch, binding, t))
        .unwrap_or(false)
}

fn repair_binding_matches(batch: &J, binding: &J, t: &J) -> bool {
    [
        model::string(binding, "token") == Some(repair_token(batch)),
        model::string(binding, "candidate") == model::string(t, "candidate"),
        model::string(binding, "base") == model::string(batch, "base"),
    ]
    .into_iter()
    .all(|ok| ok)
}

fn check_repair_live(s: &J, now: f64, live: bool, lane: &J, batch: Option<J>) -> Result<()> {
    if live {
        return check_repair_remote(lane, &batch.unwrap_or(Value::Null));
    }
    let _ = s;
    let _ = now;
    Ok(())
}

fn check_repair_remote(lane: &J, batch: &J) -> Result<()> {
    let owned = GitHub::new(lane.clone());
    let latest = validation::latest(lane, &owned)?;
    check_repair_target(&latest, batch)
}

fn check_repair_target(latest: &str, batch: &J) -> Result<()> {
    if latest != model::string(batch, "base").unwrap_or_default() {
        Err(FlowError::progress(
            "target changed before batch repair takeover",
        ))
    } else {
        Ok(())
    }
}

pub fn repair(s: &mut J, spec: &J, now: f64) -> Result<J> {
    let (t, lane) = mutable(s, spec)?;
    repair_idle(s, spec, now, t, lane)
}

fn repair_idle(s: &mut J, spec: &J, now: f64, t: J, lane: J) -> Result<J> {
    idle(&t)?;
    repair_active(s, spec, now, t, lane)
}

fn repair_active(s: &mut J, spec: &J, now: f64, t: J, lane: J) -> Result<J> {
    let batch = active(&lane, spec)?;
    repair_inputs(
        s,
        spec,
        now,
        BatchState {
            task: t,
            lane,
            batch,
        },
    )
}

fn repair_inputs(s: &mut J, spec: &J, now: f64, batch_state: BatchState) -> Result<J> {
    let operation = model::string(spec, "operation").unwrap_or_default();
    let event_id = model::string(spec, "event").unwrap_or_default();
    let op = model::dig(&batch_state.task, &["operations", &operation]).cloned();
    let event = model::dig(s, &["events", &event_id]).cloned();
    repair_validate(s, spec, now, batch_state, operation, op, event)
}

fn repair_validate(
    s: &mut J,
    spec: &J,
    now: f64,
    batch_state: BatchState,
    operation: String,
    op: Option<J>,
    event: Option<J>,
) -> Result<J> {
    if !repair_operation_valid(&batch_state.task, &op, &event) {
        return Err(FlowError::progress(
            "batch repair requires applied exact accepted result and inspected writer release",
        ));
    }
    let op = op.unwrap_or(Value::Null);
    repair_checkout(
        s,
        spec,
        now,
        batch_state,
        RepairAssignment {
            operation,
            record: op,
        },
    )
}

fn repair_operation_valid(t: &J, op: &Option<J>, event: &Option<J>) -> bool {
    [
        repair_op_accepted(op),
        repair_op_binding(op),
        repair_event_ok(t, event),
        repair_assignment_ok(t, op),
    ]
    .into_iter()
    .all(|ok| ok)
}

fn repair_op_accepted(op: &Option<J>) -> bool {
    op.as_ref()
        .map(|op| model::string(op, "phase").as_deref() == Some("accepted"))
        .unwrap_or(false)
}

fn repair_op_binding(op: &Option<J>) -> bool {
    op.as_ref()
        .and_then(|op| model::get(op, "intent"))
        .and_then(|intent| model::get(intent, "batch_repair"))
        .is_some()
}

fn repair_event_ok(t: &J, event: &Option<J>) -> bool {
    event
        .as_ref()
        .map(|event| repair_event_applied(t, event))
        .unwrap_or(false)
}

fn repair_event_applied(t: &J, event: &J) -> bool {
    [
        model::bool_or(event, "applied", false),
        model::string(event, "outcome").as_deref() == Some("complete"),
        model::get(event, "binding")
            .map(|binding| progress::same(t, binding))
            .unwrap_or(false),
    ]
    .into_iter()
    .all(|ok| ok)
}

fn repair_assignment_ok(t: &J, op: &Option<J>) -> bool {
    op.as_ref()
        .map(|op| model::string(op, "assignment") == model::string(t, "assignment"))
        .unwrap_or(false)
}

fn repair_checkout(
    s: &mut J,
    spec: &J,
    now: f64,
    batch_state: BatchState,
    repair_assignment: RepairAssignment,
) -> Result<J> {
    let checkout = repair_workspace_path(spec)?;
    repair_workspace_check(s, spec, now, batch_state, repair_assignment, checkout)
}

fn repair_workspace_path(spec: &J) -> Result<PathBuf> {
    Ok(util::resolve_path(Path::new(&progress::text(
        model::get(spec, "checkout"),
        "repair checkout",
        1000,
    )?)))
}

fn repair_workspace_check(
    s: &mut J,
    spec: &J,
    now: f64,
    batch_state: BatchState,
    repair_assignment: RepairAssignment,
    checkout: PathBuf,
) -> Result<J> {
    if Some(checkout.display().to_string())
        != model::dig_str(&repair_assignment.record, &["intent", "workspace"])
    {
        return Err(FlowError::progress(
            "repair must come from the reserved isolated workspace",
        ));
    }
    repair_head(s, spec, now, batch_state, repair_assignment, checkout)
}

fn repair_head(
    s: &mut J,
    spec: &J,
    now: f64,
    batch_state: BatchState,
    repair_assignment: RepairAssignment,
    checkout: PathBuf,
) -> Result<J> {
    let head = ledger::sha(model::get(spec, "head"))?;
    repair_evidence(s, spec, now, batch_state, repair_assignment, checkout, head)
}

fn repair_evidence(
    s: &mut J,
    spec: &J,
    now: f64,
    batch_state: BatchState,
    repair_assignment: RepairAssignment,
    checkout: PathBuf,
    head: String,
) -> Result<J> {
    let evidence = progress::text(
        model::get(spec, "evidence"),
        "batch repair verification evidence",
        1000,
    )?;
    repair_import(
        s,
        now,
        batch_state,
        repair_assignment,
        checkout,
        head,
        evidence,
    )
}

fn repair_import(
    s: &mut J,
    now: f64,
    batch_state: BatchState,
    repair_assignment: RepairAssignment,
    checkout: PathBuf,
    head: String,
    evidence: String,
) -> Result<J> {
    let (bare, parent) = git::import_repair(&batch_state.batch, &checkout, &head)?;
    repair_count(
        s,
        now,
        batch_state,
        repair_assignment,
        CandidateSource {
            checkout,
            bare,
            head,
        },
        parent,
        evidence,
    )
}

fn repair_count(
    s: &mut J,
    now: f64,
    batch_state: BatchState,
    repair_assignment: RepairAssignment,
    candidate_source: CandidateSource,
    parent: String,
    evidence: String,
) -> Result<J> {
    let count = repair_commit_count(
        &candidate_source.bare,
        &batch_state.batch,
        &candidate_source.head,
    )?;
    validate_repair_count(count)?;
    repair_phase(
        s,
        now,
        batch_state,
        repair_assignment,
        candidate_source,
        parent,
        evidence,
    )
}

fn repair_commit_count(bare: &Path, batch: &J, head: &str) -> Result<u64> {
    Ok(git::output(
        bare,
        &[
            "rev-list".to_string(),
            "--count".to_string(),
            format!(
                "{}..{head}",
                model::string(batch, "base").unwrap_or_default()
            ),
        ],
    )?
    .parse::<u64>()
    .unwrap_or(u64::MAX))
}

fn validate_repair_count(count: u64) -> Result<()> {
    if count > 256 {
        return Err(FlowError::progress(
            "repaired batch exceeds the 256-commit provenance limit",
        ));
    }
    Ok(())
}

fn repair_phase(
    s: &mut J,
    now: f64,
    batch_state: BatchState,
    repair_assignment: RepairAssignment,
    candidate_source: CandidateSource,
    parent: String,
    evidence: String,
) -> Result<J> {
    let phase = model::string(&batch_state.batch, "phase").unwrap_or_default();
    if phase == "integration_conflict" {
        return repair_conflict(
            s,
            now,
            batch_state,
            repair_assignment.operation,
            RepairRange {
                parent,
                head: candidate_source.head,
                evidence,
            },
        );
    }
    repair_gate_phase(
        s,
        now,
        batch_state,
        repair_assignment,
        candidate_source,
        parent,
        evidence,
    )
}

fn repair_gate_phase(
    s: &mut J,
    now: f64,
    batch_state: BatchState,
    repair_assignment: RepairAssignment,
    candidate_source: CandidateSource,
    parent: String,
    evidence: String,
) -> Result<J> {
    if model::string(&batch_state.batch, "phase").as_deref() == Some("gate_failed") {
        return repair_gate_failed(
            s,
            now,
            batch_state,
            repair_assignment.operation,
            candidate_source,
            parent,
            evidence,
        );
    }
    Err(FlowError::progress("batch no longer awaits this repair"))
}

fn repair_conflict(
    s: &mut J,
    now: f64,
    mut batch_state: BatchState,
    operation: String,
    repair_range: RepairRange,
) -> Result<J> {
    let conflict = model::dig(&batch_state.batch, &["integration", "conflict"])
        .cloned()
        .unwrap_or_else(|| json!({}));
    if Some(repair_range.parent.clone()) != model::string(&conflict, "parent") {
        return Err(FlowError::progress("conflict repair parent changed"));
    }
    repair_conflict_mapping(
        s,
        now,
        LaneState {
            task: batch_state.task,
            lane: batch_state.lane,
        },
        &mut batch_state.batch,
        operation,
        repair_range,
        conflict,
    )
}

fn repair_conflict_mapping(
    s: &mut J,
    now: f64,
    lane_state: LaneState,
    batch: &mut J,
    operation: String,
    repair_range: RepairRange,
    conflict: J,
) -> Result<J> {
    push_batch_mapping(
        batch,
        json!({
            "task": conflict.get("task").cloned().unwrap_or(Value::Null),
            "source": conflict.get("source").cloned().unwrap_or(Value::Null),
            "parent": repair_range.parent,
            "integrated": repair_range.head,
            "repair": operation,
            "evidence": repair_range.evidence
        }),
    );
    let offset = model::dig(batch, &["integration", "offset"])
        .and_then(Value::as_u64)
        .unwrap_or(0)
        + 1;
    batch["integration"]["offset"] = json!(offset);
    batch["integration"]["conflict"] = Value::Null;
    batch["phase"] = json!("integration_prepared");
    repair_common(
        s,
        now,
        BatchState {
            task: lane_state.task,
            lane: lane_state.lane,
            batch: batch.clone(),
        },
        operation,
        repair_range.parent,
        repair_range.head,
    )
}

fn push_batch_mapping(batch: &mut J, entry: J) {
    if let Some(mapping) = model::get_mut(batch, "integration")
        .and_then(|integration| integration.get_mut("mapping"))
        .and_then(Value::as_array_mut)
    {
        mapping.push(entry);
    }
}

fn repair_gate_failed(
    s: &mut J,
    now: f64,
    mut batch_state: BatchState,
    operation: String,
    candidate_source: CandidateSource,
    parent: String,
    evidence: String,
) -> Result<J> {
    batch_state.batch["phase"] = json!("integrated");
    batch_state.batch["integrated"] = json!(candidate_source.head.clone());
    let own = repair_own_commits(&candidate_source.bare, &parent, &candidate_source.head)?;
    repair_corrections(
        s,
        now,
        batch_state,
        operation,
        candidate_source.checkout,
        RepairRange {
            parent,
            head: candidate_source.head,
            evidence,
        },
        own,
    )
}

fn repair_own_commits(bare: &Path, parent: &str, head: &str) -> Result<Vec<String>> {
    Ok(git::output(
        bare,
        &[
            "rev-list".to_string(),
            "--reverse".to_string(),
            format!("{parent}..{head}"),
        ],
    )?
    .lines()
    .map(str::trim)
    .filter(|line| !line.is_empty())
    .map(ToString::to_string)
    .collect())
}

fn repair_corrections(
    s: &mut J,
    now: f64,
    mut batch_state: BatchState,
    operation: String,
    checkout: PathBuf,
    repair_range: RepairRange,
    own: Vec<String>,
) -> Result<J> {
    append_correction(
        &mut batch_state.batch,
        json!({
            "task": batch_state.task.get("task").cloned().unwrap_or(Value::Null),
            "checkout": checkout.display().to_string(),
            "original_base": repair_range.parent,
            "head": repair_range.head,
            "own_commits": own,
            "evidence": repair_range.evidence
        }),
    );
    repair_common(
        s,
        now,
        batch_state,
        operation,
        repair_range.parent,
        repair_range.head,
    )
}

fn append_correction(batch: &mut J, entry: J) {
    if !matches!(batch.get("corrections"), Some(Value::Array(_))) {
        batch["corrections"] = json!([]);
    }
    if let Some(corrections) = batch.get_mut("corrections").and_then(Value::as_array_mut) {
        corrections.push(entry);
    }
}

fn repair_common(
    s: &mut J,
    now: f64,
    mut batch_state: BatchState,
    operation: String,
    parent: String,
    head: String,
) -> Result<J> {
    batch_state.batch["repair_update"] = json!({
        "parent": parent,
        "head": head,
        "operation": operation
    });
    candidate(&mut batch_state.task, &head)?;
    repair_previous_pr(s, now, batch_state)
}

fn repair_previous_pr(s: &mut J, now: f64, mut batch_state: BatchState) -> Result<J> {
    if repair_pr_number(&batch_state.task) {
        batch_state.batch["previous_pr"] = model::get(&batch_state.task, "pr")
            .cloned()
            .unwrap_or(Value::Null);
    }
    repair_finish_state(s, now, batch_state)
}

fn repair_pr_number(t: &J) -> bool {
    model::get(t, "pr")
        .and_then(|pr| model::get(pr, "number"))
        .map(|value| model::truthy(Some(value)))
        .unwrap_or(false)
}

fn repair_finish_state(s: &mut J, now: f64, mut batch_state: BatchState) -> Result<J> {
    model::remove(&mut batch_state.task, "pr");
    batch_state.batch["snapshot"] = Value::Null;
    batch_state.batch["gate"] = Value::Null;
    batch_state.batch["failure_token"] = Value::Null;
    block(
        &mut batch_state.task,
        "verified repair awaiting private ref update",
        "batch-repair-finish before further integration or freezing",
    );
    batch_state.lane["active"] = batch_state.batch;
    batch_state.task["lane"] = batch_state.lane;
    save(s, batch_state.task, now, "batch_repair_update_prepared")
}

pub fn repair_finish(
    s: &mut J,
    spec: &J,
    now: f64,
    checkpoint: &mut dyn FnMut(&mut J) -> Result<()>,
) -> Result<J> {
    let (t, lane) = mutable(s, spec)?;
    repair_finish_idle(s, spec, now, checkpoint, t, lane)
}

fn repair_finish_idle(
    s: &mut J,
    spec: &J,
    now: f64,
    checkpoint: &mut dyn FnMut(&mut J) -> Result<()>,
    t: J,
    lane: J,
) -> Result<J> {
    idle(&t)?;
    repair_finish_active(s, spec, now, checkpoint, t, lane)
}

fn repair_finish_active(
    s: &mut J,
    spec: &J,
    now: f64,
    checkpoint: &mut dyn FnMut(&mut J) -> Result<()>,
    t: J,
    lane: J,
) -> Result<J> {
    let batch = active(&lane, spec)?;
    repair_finish_update(
        s,
        now,
        checkpoint,
        BatchState {
            task: t,
            lane,
            batch,
        },
    )
}

fn repair_finish_update(
    s: &mut J,
    now: f64,
    checkpoint: &mut dyn FnMut(&mut J) -> Result<()>,
    batch_state: BatchState,
) -> Result<J> {
    let update = model::get(&batch_state.batch, "repair_update")
        .cloned()
        .filter(|value| model::truthy(Some(value)))
        .ok_or_else(|| FlowError::progress("no prepared private repair update"))?;
    repair_finish_actual(s, now, checkpoint, batch_state, update)
}

fn repair_finish_actual(
    s: &mut J,
    now: f64,
    checkpoint: &mut dyn FnMut(&mut J) -> Result<()>,
    batch_state: BatchState,
    update: J,
) -> Result<J> {
    let bare = snapshot_bare(&batch_state.batch);
    let actual = git::output(
        &bare,
        &["rev-parse".to_string(), "refs/herdr/candidate".to_string()],
    )?;
    repair_finish_verify(s, now, checkpoint, batch_state, update, bare, actual)
}

fn repair_finish_verify(
    s: &mut J,
    now: f64,
    checkpoint: &mut dyn FnMut(&mut J) -> Result<()>,
    batch_state: BatchState,
    update: J,
    bare: PathBuf,
    actual: String,
) -> Result<J> {
    let parent = model::string(&update, "parent").unwrap_or_default();
    let head = model::string(&update, "head").unwrap_or_default();
    if !repair_ref_ok(&actual, &parent, &head) {
        return Err(FlowError::progress(
            "private ref changed outside repair journal",
        ));
    }
    repair_finish_ref(
        s,
        now,
        checkpoint,
        batch_state,
        bare,
        RepairRef {
            actual,
            parent,
            head,
        },
    )
}

fn repair_ref_ok(actual: &str, parent: &str, head: &str) -> bool {
    actual == parent || actual == head
}

fn repair_finish_ref(
    s: &mut J,
    now: f64,
    checkpoint: &mut dyn FnMut(&mut J) -> Result<()>,
    batch_state: BatchState,
    bare: PathBuf,
    repair_ref: RepairRef,
) -> Result<J> {
    if repair_ref.actual == repair_ref.parent {
        return repair_finish_update_ref(
            s,
            now,
            checkpoint,
            batch_state,
            bare,
            repair_ref.parent,
            repair_ref.head,
        );
    }
    repair_finish_done(s, now, checkpoint, batch_state)
}

fn repair_finish_update_ref(
    s: &mut J,
    now: f64,
    checkpoint: &mut dyn FnMut(&mut J) -> Result<()>,
    batch_state: BatchState,
    bare: PathBuf,
    parent: String,
    head: String,
) -> Result<J> {
    git::run(
        &bare,
        &[
            "update-ref".to_string(),
            "refs/herdr/candidate".to_string(),
            head.clone(),
            parent,
        ],
        None,
        None,
        &std::collections::BTreeMap::new(),
        false,
    )?;
    repair_finish_done(s, now, checkpoint, batch_state)
}

fn repair_finish_done(
    s: &mut J,
    now: f64,
    checkpoint: &mut dyn FnMut(&mut J) -> Result<()>,
    mut batch_state: BatchState,
) -> Result<J> {
    model::remove(&mut batch_state.batch, "repair_update");
    batch_state.lane["active"] = batch_state.batch;
    batch_state.task["lane"] = batch_state.lane;
    batch_state.task = save(s, batch_state.task, now, "batch_repair_ref_updated")?;
    repair_finish_checkpoint(s, checkpoint, batch_state.task)
}

fn repair_finish_checkpoint(
    s: &mut J,
    checkpoint: &mut dyn FnMut(&mut J) -> Result<()>,
    t: J,
) -> Result<J> {
    checkpoint(s)?;
    Ok(t)
}

pub fn rebase(s: &mut J, spec: &J, now: f64, api: Option<&dyn GitHubApi>) -> Result<J> {
    let (t, lane) = mutable(s, spec)?;
    rebase_idle(s, spec, now, api, t, lane)
}

fn rebase_idle(
    s: &mut J,
    spec: &J,
    now: f64,
    api: Option<&dyn GitHubApi>,
    t: J,
    lane: J,
) -> Result<J> {
    idle(&t)?;
    rebase_active(s, spec, now, api, t, lane)
}

fn rebase_active(
    s: &mut J,
    spec: &J,
    now: f64,
    api: Option<&dyn GitHubApi>,
    t: J,
    lane: J,
) -> Result<J> {
    let batch = active(&lane, spec)?;
    rebase_guard(
        s,
        spec,
        now,
        api,
        BatchState {
            task: t,
            lane,
            batch,
        },
    )
}

fn rebase_guard(
    s: &mut J,
    spec: &J,
    now: f64,
    api: Option<&dyn GitHubApi>,
    batch_state: BatchState,
) -> Result<J> {
    if rebase_uncertain(&batch_state.task, &batch_state.batch) {
        return Err(FlowError::progress(
            "reconcile uncertain repair/publication before rebuilding",
        ));
    }
    rebase_operations(s, spec, now, api, batch_state)
}

fn rebase_uncertain(t: &J, batch: &J) -> bool {
    [
        model::get(batch, "repair_update").is_some(),
        rebase_publication_sending(t),
    ]
    .into_iter()
    .any(|uncertain| uncertain)
}

fn rebase_publication_sending(t: &J) -> bool {
    model::get(t, "pr")
        .and_then(|pr| model::dig_str(pr, &["publication", "phase"]))
        .as_deref()
        == Some("sending")
}

fn rebase_operations(
    s: &mut J,
    spec: &J,
    now: f64,
    api: Option<&dyn GitHubApi>,
    batch_state: BatchState,
) -> Result<J> {
    if rebase_operations_pending(&batch_state.task, &batch_state.batch) {
        return Err(FlowError::progress(
            "import or reconcile the existing batch repair before rebuilding",
        ));
    }
    rebase_gate(s, spec, now, api, batch_state)
}

fn rebase_operations_pending(t: &J, batch: &J) -> bool {
    model::get(t, "operations")
        .and_then(Value::as_object)
        .map(|operations| {
            operations
                .values()
                .any(|op| rebase_operation_pending(op, batch, t))
        })
        .unwrap_or(false)
}

fn rebase_operation_pending(op: &J, batch: &J, t: &J) -> bool {
    let Some(binding) =
        model::get(op, "intent").and_then(|intent| model::get(intent, "batch_repair"))
    else {
        return false;
    };
    [
        rebase_op_phase(op),
        model::string(binding, "batch") == model::string(batch, "id"),
        model::string(binding, "candidate") == model::string(t, "candidate"),
        model::string(binding, "base") == model::string(batch, "base"),
    ]
    .into_iter()
    .all(|pending| pending)
}

fn rebase_op_phase(op: &J) -> bool {
    matches!(
        model::string(op, "phase").as_deref(),
        Some("prepared" | "sending" | "submitted" | "accepted")
    )
}

fn rebase_gate(
    s: &mut J,
    spec: &J,
    now: f64,
    api: Option<&dyn GitHubApi>,
    batch_state: BatchState,
) -> Result<J> {
    if rebase_gate_sending(&batch_state.batch) {
        return rebase_gate_inspect(s, spec, now, api, batch_state);
    }
    rebase_api(s, spec, now, api, batch_state)
}

fn rebase_gate_sending(batch: &J) -> bool {
    model::get(batch, "gate")
        .and_then(|gate| model::string(gate, "phase"))
        .as_deref()
        == Some("sending")
}

fn rebase_gate_inspect(
    s: &mut J,
    spec: &J,
    now: f64,
    api: Option<&dyn GitHubApi>,
    mut batch_state: BatchState,
) -> Result<J> {
    batch_state.batch["gate"]["inspection"] =
        gate::quiescent(&batch_state.lane, &batch_state.batch)?;
    rebase_api(s, spec, now, api, batch_state)
}

fn rebase_api(
    s: &mut J,
    spec: &J,
    now: f64,
    api: Option<&dyn GitHubApi>,
    batch_state: BatchState,
) -> Result<J> {
    let owned;
    let api: &dyn GitHubApi = if let Some(api) = api {
        api
    } else {
        owned = GitHub::new(batch_state.lane.clone());
        &owned
    };
    rebase_pr(s, spec, now, batch_state, api)
}

fn rebase_pr(
    s: &mut J,
    spec: &J,
    now: f64,
    batch_state: BatchState,
    api: &dyn GitHubApi,
) -> Result<J> {
    if rebase_pr_number(&batch_state.task) {
        return rebase_pr_remote(s, spec, now, batch_state, api);
    }
    rebase_latest(s, spec, now, batch_state, api)
}

fn rebase_pr_number(t: &J) -> bool {
    model::get(t, "pr")
        .and_then(|pr| model::int(pr, "number"))
        .filter(|number| *number > 0)
        .is_some()
}

fn rebase_pr_remote(
    s: &mut J,
    spec: &J,
    now: f64,
    batch_state: BatchState,
    api: &dyn GitHubApi,
) -> Result<J> {
    let (r, remote) = rebase_remote_pr(api, &batch_state.task)?;
    rebase_pr_check(s, spec, now, batch_state, api, r, remote)
}

fn rebase_remote_pr(api: &dyn GitHubApi, t: &J) -> Result<(J, J)> {
    let r = model::get(t, "pr").cloned().unwrap_or_else(|| json!({}));
    let number = model::int(&r, "number").unwrap_or(0);
    let remote = api.get(&format!("pulls/{number}"))?;
    Ok((r, remote))
}

fn rebase_pr_check(
    s: &mut J,
    spec: &J,
    now: f64,
    mut batch_state: BatchState,
    api: &dyn GitHubApi,
    r: J,
    remote: J,
) -> Result<J> {
    if !rebase_pr_unchanged(&r, &remote) {
        return Err(FlowError::progress(
            "published batch changed or landed; reconcile before rebuilding",
        ));
    }
    batch_state.batch["previous_pr"] = r;
    rebase_latest(s, spec, now, batch_state, api)
}

fn rebase_pr_unchanged(r: &J, remote: &J) -> bool {
    [
        ledger::pr_identity(r, remote),
        model::dig_str(remote, &["head", "sha"]) == model::string(r, "head"),
        model::string(remote, "state").as_deref() == Some("open"),
        !model::bool_or(remote, "merged", false),
    ]
    .into_iter()
    .all(|ok| ok)
}

fn rebase_latest(
    s: &mut J,
    spec: &J,
    now: f64,
    batch_state: BatchState,
    api: &dyn GitHubApi,
) -> Result<J> {
    let base = validation::latest(&batch_state.lane, api)?;
    rebase_changed(s, spec, now, batch_state, api, base)
}

fn rebase_changed(
    s: &mut J,
    spec: &J,
    now: f64,
    batch_state: BatchState,
    api: &dyn GitHubApi,
    base: String,
) -> Result<J> {
    if base == model::string(&batch_state.batch, "base").unwrap_or_default() {
        return Err(FlowError::progress("target has not changed"));
    }
    rebase_ancestry(s, spec, now, batch_state, api, base)
}

fn rebase_ancestry(
    s: &mut J,
    spec: &J,
    now: f64,
    batch_state: BatchState,
    api: &dyn GitHubApi,
    base: String,
) -> Result<J> {
    validation::landing_ancestry(&batch_state.lane, &base, api)?;
    rebase_versions(s, spec, now, batch_state, base)
}

fn rebase_versions(
    s: &mut J,
    spec: &J,
    now: f64,
    batch_state: BatchState,
    base: String,
) -> Result<J> {
    let BatchState {
        task: t,
        lane,
        batch,
    } = batch_state;

    let versions_len = model::array_field(&batch, "versions")
        .map(Vec::len)
        .unwrap_or(0);
    if versions_len >= 16 {
        return Err(FlowError::progress(
            "batch rebuild limit reached; retain ownership and inspect",
        ));
    }
    let batch = rebase_apply(batch, &lane, now, &base);
    rebase_after_apply(
        s,
        spec,
        now,
        BatchState {
            task: t,
            lane,
            batch,
        },
        base,
    )
}

fn rebase_apply(mut batch: J, lane: &J, now: f64, base: &str) -> J {
    let version = json!({
        "base": batch.get("base").cloned().unwrap_or(Value::Null),
        "snapshot": batch.get("snapshot").cloned().unwrap_or(Value::Null),
        "gate": batch.get("gate").cloned().unwrap_or(Value::Null),
        "workspace": batch.get("workspace").cloned().unwrap_or(Value::Null),
        "integration": batch.get("integration").cloned().unwrap_or(Value::Null)
    });
    push_version(&mut batch, version);
    let revision = model::int(&batch, "revision").unwrap_or(0) + 1;
    batch["revision"] = json!(revision);
    batch["base"] = json!(base);
    batch["phase"] = json!("integration_prepared");
    batch["snapshot"] = Value::Null;
    batch["gate"] = Value::Null;
    batch["failure_token"] = Value::Null;
    batch["progress_deadline"] =
        json!(now + model::number(lane, "progress_timeout").unwrap_or(600.0));
    batch["workspace"] = json!(rebase_workspace(lane, &batch, revision));
    batch["integration"] =
        json!({"ready": false, "offset": 0, "mapping": [], "current": null, "conflict": null});
    batch
}

fn push_version(batch: &mut J, version: J) {
    if !matches!(batch.get("versions"), Some(Value::Array(_))) {
        batch["versions"] = json!([]);
    }
    if let Some(versions) = batch.get_mut("versions").and_then(Value::as_array_mut) {
        versions.push(version);
    }
}

fn rebase_workspace(lane: &J, batch: &J, revision: i64) -> String {
    Path::new(&model::string(lane, "workspace_root").unwrap_or_default())
        .join(format!(
            "{}-r{revision}",
            model::string(batch, "id").unwrap_or_default()
        ))
        .display()
        .to_string()
}

fn rebase_after_apply(
    s: &mut J,
    spec: &J,
    now: f64,
    mut batch_state: BatchState,
    base: String,
) -> Result<J> {
    let _ = spec;
    candidate(&mut batch_state.task, &base)?;
    rebase_store(s, now, batch_state)
}

fn rebase_store(s: &mut J, now: f64, mut batch_state: BatchState) -> Result<J> {
    model::remove(&mut batch_state.task, "pr");
    block(
        &mut batch_state.task,
        "same frozen members need integration on the new target",
        "batch-integrate then a new snapshot and trusted gate",
    );
    batch_state.lane["active"] = batch_state.batch;
    batch_state.task["lane"] = batch_state.lane;
    save(s, batch_state.task, now, "batch_base_changed")
}

pub fn stall(t: &J, now: f64) -> Option<String> {
    let lane = model::get(t, "lane").filter(|value| model::truthy(Some(value)))?;
    stall_active(t, lane, now)
}

fn stall_active(t: &J, lane: &J, now: f64) -> Option<String> {
    let batch = model::get(lane, "active").filter(|value| model::truthy(Some(value)))?;
    stall_batch(t, batch, now)
}

fn stall_batch(t: &J, batch: &J, now: f64) -> Option<String> {
    let phase = model::string(batch, "phase").unwrap_or_default();
    if stall_stuck(t, &phase, batch, now) {
        Some(stall_code(&phase))
    } else {
        None
    }
}

fn stall_stuck(t: &J, phase: &str, batch: &J, now: f64) -> bool {
    [
        stall_phase(phase),
        model::string(t, "phase").as_deref() == Some("blocked"),
        now >= model::number(batch, "progress_deadline").unwrap_or(f64::INFINITY),
    ]
    .into_iter()
    .all(|stuck| stuck)
}

fn stall_phase(phase: &str) -> bool {
    matches!(
        phase,
        "integration_prepared"
            | "integrating"
            | "integrated"
            | "snapshot_frozen"
            | "gate_uncertain"
    )
}

fn stall_code(phase: &str) -> String {
    if phase == "gate_uncertain" {
        "batch_operation_unreconciled".to_string()
    } else {
        "batch_successor_unaccepted".to_string()
    }
}

type BatchActionFn = fn(&mut J, &J, f64, &mut dyn FnMut(&mut J) -> Result<()>) -> Result<J>;

const BATCH_ACTIONS: &[(&str, BatchActionFn)] = &[
    ("batch-integrate", batch_action_integrate),
    ("batch-repair-finish", batch_action_repair_finish),
    ("batch-init", batch_action_init),
    ("batch-offer", batch_action_offer),
    ("batch-freeze", batch_action_freeze),
    ("batch-snapshot", batch_action_snapshot),
    ("batch-gate-begin", batch_action_gate_begin),
    ("batch-gate-record", batch_action_gate_record),
    ("batch-reconcile", batch_action_reconcile),
    ("batch-repair", batch_action_repair),
    ("batch-rebase", batch_action_rebase),
];

pub fn action(
    s: &mut J,
    command: &str,
    spec: &J,
    now: f64,
    checkpoint: &mut dyn FnMut(&mut J) -> Result<()>,
) -> Result<J> {
    let handler = find_action(command)
        .ok_or_else(|| FlowError::progress(format!("unknown batch action: {command}")))?;
    handler(s, spec, now, checkpoint)
}

fn find_action(command: &str) -> Option<BatchActionFn> {
    BATCH_ACTIONS
        .iter()
        .find(|(name, _)| *name == command)
        .map(|(_, handler)| *handler)
}

fn batch_action_integrate(
    s: &mut J,
    spec: &J,
    now: f64,
    checkpoint: &mut dyn FnMut(&mut J) -> Result<()>,
) -> Result<J> {
    integrate(s, spec, now, checkpoint, None)
}

fn batch_action_repair_finish(
    s: &mut J,
    spec: &J,
    now: f64,
    checkpoint: &mut dyn FnMut(&mut J) -> Result<()>,
) -> Result<J> {
    repair_finish(s, spec, now, checkpoint)
}

fn batch_action_init(
    s: &mut J,
    spec: &J,
    now: f64,
    _checkpoint: &mut dyn FnMut(&mut J) -> Result<()>,
) -> Result<J> {
    init(s, spec, now)
}

fn batch_action_offer(
    s: &mut J,
    spec: &J,
    now: f64,
    _checkpoint: &mut dyn FnMut(&mut J) -> Result<()>,
) -> Result<J> {
    offer(s, spec, now)
}

fn batch_action_freeze(
    s: &mut J,
    spec: &J,
    now: f64,
    _checkpoint: &mut dyn FnMut(&mut J) -> Result<()>,
) -> Result<J> {
    freeze(s, spec, now, None)
}

fn batch_action_snapshot(
    s: &mut J,
    spec: &J,
    now: f64,
    _checkpoint: &mut dyn FnMut(&mut J) -> Result<()>,
) -> Result<J> {
    snapshot(s, spec, now, None)
}

fn batch_action_gate_begin(
    s: &mut J,
    spec: &J,
    now: f64,
    _checkpoint: &mut dyn FnMut(&mut J) -> Result<()>,
) -> Result<J> {
    gate_begin(s, spec, now, None)
}

fn batch_action_gate_record(
    s: &mut J,
    spec: &J,
    now: f64,
    _checkpoint: &mut dyn FnMut(&mut J) -> Result<()>,
) -> Result<J> {
    gate_record(s, spec, now, None)
}

fn batch_action_reconcile(
    s: &mut J,
    spec: &J,
    now: f64,
    _checkpoint: &mut dyn FnMut(&mut J) -> Result<()>,
) -> Result<J> {
    reconcile(s, spec, now, None)
}

fn batch_action_repair(
    s: &mut J,
    spec: &J,
    now: f64,
    _checkpoint: &mut dyn FnMut(&mut J) -> Result<()>,
) -> Result<J> {
    repair(s, spec, now)
}

fn batch_action_rebase(
    s: &mut J,
    spec: &J,
    now: f64,
    _checkpoint: &mut dyn FnMut(&mut J) -> Result<()>,
) -> Result<J> {
    rebase(s, spec, now, None)
}

// Keep module-internal public helpers discoverable for tests.
#[allow(dead_code)]
fn _path_buf(path: &str) -> PathBuf {
    PathBuf::from(path)
}

#[cfg(test)]
mod audit_tests {
    use super::*;

    struct NoGitHub;

    impl GitHubApi for NoGitHub {
        fn get(&self, _path: &str) -> Result<J> {
            panic!("member state transitions must not access GitHub")
        }
    }

    #[test]
    fn intake_receipts_are_distinct_at_the_same_timestamp() {
        let mut state = json!({"tasks":{}});
        let mut member = |name| {
            offer_member_store(
                &mut state,
                10.0,
                LaneState {
                    task: json!({"task":"lane","state_version":1}),
                    lane: json!({"pending":[]}),
                },
                json!({"task":name,"epoch":1}),
                OfferedSource {
                    source: json!({"head":"source-head"}),
                    checks: vec![],
                },
                vec![],
                "evidence".to_owned(),
            )
            .unwrap()["lane"]["pending"][0]
                .clone()
        };
        let first = member("first");
        let second = member("second");
        assert_eq!(
            state["tasks"]["first"]["batch_delivery"]["receipt_id"],
            first["receipt_id"]
        );
        assert_eq!(
            state["tasks"]["second"]["batch_delivery"]["receipt_id"],
            second["receipt_id"]
        );
        assert_ne!(first["receipt_id"], second["receipt_id"]);
        assert_eq!(first["accepted_at"], second["accepted_at"]);
    }

    #[test]
    fn intake_receipt_survives_freeze_and_landing_without_migrating_legacy_records() {
        for receipt in [None, Some("accepted-source-receipt")] {
            let mut member = json!({"task":"source","head":"source-head","acceptance":[]});
            let mut delivery = json!({"owner":"lane","phase":"accepted","head":"source-head"});
            if let Some(receipt) = receipt {
                member["receipt_id"] = json!(receipt);
                delivery["receipt_id"] = json!(receipt);
                delivery["claimed_by"] = json!({"task":"successor","epoch":1});
            }
            let consumed = delivery.get("claimed_by").cloned();
            let mut s = json!({"tasks":{"source":{"task":"source","state_version":1,
                "history":[],"batch_delivery":delivery}}});
            let batch =
                json!({"id":"batch-one","base":"base","snapshot":{"head":"final","tree":"tree"}});
            freeze_member(&mut s, &member, &batch, 11.0).unwrap();
            s = serde_json::from_slice(&serde_json::to_vec(&s).unwrap()).unwrap();
            assert_eq!(
                s["tasks"]["source"]["batch_delivery"].get("receipt_id"),
                member.get("receipt_id")
            );
            reconcile_member(
                &mut s,
                &member,
                &batch,
                &json!({"commit":"landed"}),
                ReconcileCtx {
                    now: 12.0,
                    api: &NoGitHub,
                },
            )
            .unwrap();
            s = serde_json::from_slice(&serde_json::to_vec(&s).unwrap()).unwrap();
            assert_eq!(s["tasks"]["source"]["batch_delivery"]["phase"], "landed");
            assert_eq!(
                s["tasks"]["source"]["batch_delivery"].get("claimed_by"),
                consumed.as_ref()
            );
            assert_eq!(
                s["tasks"]["source"]["batch_delivery"].get("receipt_id"),
                member.get("receipt_id")
            );
        }
    }

    #[test]
    fn repair_and_rebase_history_arrays_are_initialized() {
        let mut batch = json!({});
        append_correction(&mut batch, json!({"head": "repair-head"}));
        assert_eq!(batch["corrections"].as_array().unwrap().len(), 1);
        push_version(&mut batch, json!({"base": "old-base"}));
        assert_eq!(batch["versions"].as_array().unwrap().len(), 1);
    }
}
