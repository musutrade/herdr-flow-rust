//! Read-only batch checks shared by lane orchestration and PR publication.
//!
//! This module does not import either state machine. Both entrypoints use the
//! same writer, dependency, baseline and trusted-result checks before
//! publication.

use serde_json::{json, Map, Value};

use crate::ci::watch::GitHubApi;
use crate::error::{FlowError, Result};
use crate::model::{self, J};
use crate::progress;
use crate::util::pyjson::quote_component;

use super::gate;

pub fn idle(t: &J) -> Result<()> {
    let writer = model::get(t, "writer")
        .map(|value| model::truthy(Some(value)))
        .unwrap_or(false);
    let pending = model::array_field(t, "pending")
        .map(|items| !items.is_empty())
        .unwrap_or(false);
    if writer || pending {
        return Err(FlowError::progress(
            "apply pending results and inspect writer release before changing batch inputs",
        ));
    }
    Ok(())
}

fn validate_frozen_member(mt: &J, member: &J) -> Result<()> {
    ensure_frozen_identity(mt, member)?;
    validate_released_member(mt)
}

fn ensure_frozen_identity(mt: &J, member: &J) -> Result<()> {
    if frozen_identity_ok(mt, member) {
        Ok(())
    } else {
        Err(FlowError::progress(
            "frozen member identity, epoch or candidate changed",
        ))
    }
}

fn frozen_identity_ok(mt: &J, member: &J) -> bool {
    [
        frozen_candidate_ok(mt, member),
        frozen_epoch_ok(mt, member),
        frozen_running_ok(mt),
    ]
    .into_iter()
    .all(|ok| ok)
}

fn frozen_candidate_ok(mt: &J, member: &J) -> bool {
    model::get(mt, "candidate") == model::get(member, "head")
}

fn frozen_epoch_ok(mt: &J, member: &J) -> bool {
    model::get(mt, "epoch") == model::get(member, "epoch")
}

fn frozen_running_ok(mt: &J) -> bool {
    model::string(mt, "control").as_deref() == Some("running")
}

fn validate_released_member(mt: &J) -> Result<()> {
    idle(mt)?;
    validate_member_checks(mt)
}

fn validate_member_checks(mt: &J) -> Result<()> {
    ensure_released(mt)?;
    ensure_no_blocking_findings(mt)
}

fn ensure_released(mt: &J) -> Result<()> {
    if released_member_ok(mt) {
        Ok(())
    } else {
        Err(FlowError::progress(
            "frozen member must remain released at its lane handoff without a PR or lane",
        ))
    }
}

fn released_member_ok(mt: &J) -> bool {
    [
        !model::truthy(model::get(mt, "pr")),
        !model::truthy(model::get(mt, "lane")),
        model::string(mt, "phase").as_deref() == Some("blocked"),
    ]
    .into_iter()
    .all(|ok| ok)
}

fn ensure_no_blocking_findings(mt: &J) -> Result<()> {
    if unresolved_findings(mt) {
        Err(FlowError::progress(
            "a frozen member has unresolved blocking review findings",
        ))
    } else {
        Ok(())
    }
}

pub(crate) fn unresolved_findings(value: &J) -> bool {
    model::get(value, "findings")
        .and_then(Value::as_object)
        .map(findings_blocking)
        .unwrap_or(false)
}

fn findings_blocking(findings: &Map<String, Value>) -> bool {
    findings.values().any(finding_blocking)
}

fn finding_blocking(finding: &J) -> bool {
    model::bool_or(finding, "blocking", false) && finding_unresolved(finding)
}

fn finding_unresolved(finding: &J) -> bool {
    finding.get("resolved").map(Value::is_null).unwrap_or(true)
}

fn validate_original_base(api: &dyn GitHubApi, member: &J, batch: &J) -> Result<()> {
    let original = model::string(member, "original_base").unwrap_or_default();
    if original_declared(member, batch, &original) {
        return Ok(());
    }
    validate_original_ancestry(api, batch, &original)
}

fn original_declared(member: &J, batch: &J, original: &str) -> bool {
    [
        original_is_base(original, batch),
        original_is_dependency(original, member),
    ]
    .into_iter()
    .any(|declared| declared)
}

fn original_is_base(original: &str, batch: &J) -> bool {
    Some(original) == model::string(batch, "base").as_deref()
}

fn original_is_dependency(original: &str, member: &J) -> bool {
    model::array_field(member, "dependencies")
        .map(|dependencies| {
            dependencies
                .iter()
                .any(|dependency| model::string(dependency, "head").as_deref() == Some(original))
        })
        .unwrap_or(false)
}

fn validate_original_ancestry(api: &dyn GitHubApi, batch: &J, original: &str) -> Result<()> {
    let ancestry = api.get(&format!(
        "compare/{}...{}",
        original,
        model::string(batch, "base").unwrap_or_default()
    ))?;
    original_on_target(&ancestry, original)
}

fn original_on_target(ancestry: &J, original: &str) -> Result<()> {
    if original_target_ok(ancestry, original) {
        Ok(())
    } else {
        Err(FlowError::progress(
            "original base is neither on the target nor an exact declared dependency head",
        ))
    }
}

fn original_target_ok(ancestry: &J, original: &str) -> bool {
    [
        accepted_status(ancestry),
        merge_base_matches(ancestry, original),
    ]
    .into_iter()
    .all(|ok| ok)
}

fn accepted_status(value: &J) -> bool {
    matches!(
        model::string(value, "status").as_deref(),
        Some("ahead" | "identical")
    )
}

fn merge_base_matches(value: &J, sha: &str) -> bool {
    model::dig_str(value, &["merge_base_commit", "sha"]).as_deref() == Some(sha)
}

fn validate_dependency_identity(s: &J, dep: &J) -> Result<J> {
    let task_name = model::string(dep, "task").unwrap_or_default();
    let dt = model::dig(s, &["tasks", &task_name])
        .cloned()
        .unwrap_or_else(|| json!({}));
    if dependency_changed(&dt, dep) {
        return Err(FlowError::progress("frozen dependency changed"));
    }
    Ok(dt)
}

fn dependency_changed(dt: &J, dep: &J) -> bool {
    [
        dependency_candidate_changed(dt, dep),
        dependency_cancelled(dt),
    ]
    .into_iter()
    .any(|changed| changed)
}

fn dependency_candidate_changed(dt: &J, dep: &J) -> bool {
    model::string(dt, "candidate") != model::string(dep, "head")
}

fn dependency_cancelled(dt: &J) -> bool {
    model::string(dt, "control").as_deref() == Some("cancelled")
}

fn validate_dependency_landing(api: &dyn GitHubApi, dt: &J, dep: &J, batch: &J) -> Result<()> {
    let landing = dependency_landing(dt, dep)?;
    validate_landing_ancestry(api, &landing, batch)
}

fn dependency_landing(dt: &J, dep: &J) -> Result<J> {
    let landing = require_landing(landing_candidate(dt), dep)?;
    ensure_pinned_return(&landing, dep)
}

fn ensure_pinned_return(landing: &J, dep: &J) -> Result<J> {
    ensure_pinned(landing, dep)?;
    Ok(landing.clone())
}

fn landing_candidate(dt: &J) -> Option<J> {
    let delivery = model::get(dt, "batch_delivery")
        .cloned()
        .unwrap_or_else(|| json!({}));
    model::get(&delivery, "landing")
        .cloned()
        .or_else(|| pr_landing(dt))
}

fn pr_landing(dt: &J) -> Option<J> {
    model::get(dt, "pr")
        .filter(|value| model::truthy(Some(value)))
        .and_then(|pr| model::get(pr, "landing"))
        .cloned()
}

fn require_landing(landing: Option<J>, dep: &J) -> Result<J> {
    let landing = landing.filter(|value| !value.is_null());
    if landing_head_ok(&landing, dep) {
        Ok(landing.unwrap_or(Value::Null))
    } else {
        Err(FlowError::progress(
            "dependency is not landed or earlier in the frozen batch",
        ))
    }
}

fn landing_head_ok(landing: &Option<J>, dep: &J) -> bool {
    landing
        .as_ref()
        .map(|landing| model::string(landing, "head") == model::string(dep, "head"))
        .unwrap_or(false)
}

fn ensure_pinned(landing: &J, dep: &J) -> Result<()> {
    if pinned_changed(landing, dep) {
        Err(FlowError::progress("pinned dependency landing changed"))
    } else {
        Ok(())
    }
}

fn pinned_changed(landing: &J, dep: &J) -> bool {
    model::get(dep, "landing")
        .filter(|value| model::truthy(Some(value)))
        .map(|pinned| model::string(pinned, "commit") != model::string(landing, "commit"))
        .unwrap_or(false)
}

fn validate_landing_ancestry(api: &dyn GitHubApi, landing: &J, batch: &J) -> Result<()> {
    let compare = fetch_landing_compare(api, landing, batch)?;
    let commit = model::string(landing, "commit").unwrap_or_default();
    ensure_landing_on_target(&compare, &commit)
}

fn fetch_landing_compare(api: &dyn GitHubApi, landing: &J, batch: &J) -> Result<J> {
    let commit = model::string(landing, "commit").unwrap_or_default();
    api.get(&format!(
        "compare/{commit}...{}",
        model::string(batch, "base").unwrap_or_default()
    ))
}

fn ensure_landing_on_target(compare: &J, commit: &str) -> Result<()> {
    if landing_target_ok(compare, commit) {
        Ok(())
    } else {
        Err(FlowError::progress(
            "dependency landing is absent from the frozen target",
        ))
    }
}

fn landing_target_ok(compare: &J, commit: &str) -> bool {
    [
        accepted_status(compare),
        merge_base_matches(compare, commit),
    ]
    .into_iter()
    .all(|ok| ok)
}

pub fn dependencies(s: &J, lane: &J, batch: &J, api: &dyn GitHubApi) -> Result<()> {
    let _ = lane;
    let Some(members) = model::array_field(batch, "members") else {
        return Err(FlowError::progress("batch members are unavailable"));
    };
    let mut seen = std::collections::BTreeSet::new();
    members
        .iter()
        .try_for_each(|member| validate_member(s, member, batch, api, &mut seen))
}

fn validate_member(
    s: &J,
    member: &J,
    batch: &J,
    api: &dyn GitHubApi,
    seen: &mut std::collections::BTreeSet<String>,
) -> Result<()> {
    let mt = member_state(s, member);
    validate_frozen_member(&mt, member)?;
    progress::claim::held(s, &mt)?;
    validate_member_rest(s, member, batch, api, seen)
}

fn member_state(s: &J, member: &J) -> J {
    model::string(member, "task")
        .and_then(|task| model::dig(s, &["tasks", &task]).cloned())
        .unwrap_or_else(|| json!({}))
}

fn validate_member_rest(
    s: &J,
    member: &J,
    batch: &J,
    api: &dyn GitHubApi,
    seen: &mut std::collections::BTreeSet<String>,
) -> Result<()> {
    validate_original_base(api, member, batch)?;
    validate_member_dependencies(s, member, batch, api, seen)
}

fn validate_member_dependencies(
    s: &J,
    member: &J,
    batch: &J,
    api: &dyn GitHubApi,
    seen: &mut std::collections::BTreeSet<String>,
) -> Result<()> {
    let Some(deps) = model::array_field(member, "dependencies") else {
        return validate_member_seen(member, seen);
    };
    deps.iter()
        .try_for_each(|dep| validate_dependency_once(s, dep, batch, api, seen))?;
    validate_member_seen(member, seen)
}

fn validate_member_seen(member: &J, seen: &mut std::collections::BTreeSet<String>) -> Result<()> {
    seen.insert(model::string(member, "task").unwrap_or_default());
    Ok(())
}

fn validate_dependency_once(
    s: &J,
    dep: &J,
    batch: &J,
    api: &dyn GitHubApi,
    seen: &mut std::collections::BTreeSet<String>,
) -> Result<()> {
    let dt = validate_dependency_identity(s, dep)?;
    validate_dependency_if_new(api, &dt, dep, batch, seen)
}

fn validate_dependency_if_new(
    api: &dyn GitHubApi,
    dt: &J,
    dep: &J,
    batch: &J,
    seen: &std::collections::BTreeSet<String>,
) -> Result<()> {
    if seen.contains(&model::string(dep, "task").unwrap_or_default()) {
        return Ok(());
    }
    validate_dependency_landing(api, dt, dep, batch)
}

pub fn latest(lane: &J, api: &dyn GitHubApi) -> Result<String> {
    let base_ref = model::string(lane, "base_ref").unwrap_or_default();
    let payload = api.get(&format!("branches/{}", quote_component(&base_ref)))?;
    let value = model::dig_str(&payload, &["commit", "sha"]).unwrap_or_default();
    if !model::is_full_sha(&value) {
        return Err(FlowError::progress("PR provenance requires full SHAs"));
    }
    Ok(value)
}

pub fn landing_ancestry(lane: &J, base: &str, api: &dyn GitHubApi) -> Result<()> {
    let Some(history) = model::array_field(lane, "history").filter(|items| !items.is_empty())
    else {
        return Ok(());
    };
    let previous = history
        .last()
        .and_then(|entry| model::dig_str(entry, &["landing", "commit"]))
        .unwrap_or_default();
    let ancestry = api.get(&format!("compare/{previous}...{base}"))?;
    let status = model::string(&ancestry, "status").unwrap_or_default();
    if !matches!(status.as_str(), "ahead" | "identical")
        || model::dig_str(&ancestry, &["merge_base_commit", "sha"]).as_deref()
            != Some(previous.as_str())
    {
        return Err(FlowError::progress(
            "previous batch landing is absent from the latest target; retain its slot",
        ));
    }
    Ok(())
}

pub fn publication_check(s: &J, t: &J, now: f64, api: Option<&dyn GitHubApi>) -> Result<()> {
    let lane = publication_prelude(t)?;
    publication_lane(s, t, now, api, lane)
}

fn publication_prelude(t: &J) -> Result<Option<J>> {
    progress::require_unowned(t, false)?;
    publication_checks(t)
}

fn publication_checks(t: &J) -> Result<Option<J>> {
    idle(t)?;
    publication_findings(t)
}

fn publication_findings(t: &J) -> Result<Option<J>> {
    if unresolved_findings(t) {
        return Err(FlowError::progress(
            "blocking review findings remain unresolved",
        ));
    }
    Ok(model::get(t, "lane")
        .filter(|value| model::truthy(Some(value)))
        .cloned())
}

fn publication_lane(
    s: &J,
    t: &J,
    now: f64,
    api: Option<&dyn GitHubApi>,
    lane: Option<J>,
) -> Result<()> {
    let Some(lane) = lane else {
        return Ok(());
    };
    let batch = publication_batch(&lane)?;
    publication_target(s, t, now, api, &lane, &batch)
}

fn publication_batch(lane: &J) -> Result<J> {
    model::get(lane, "active")
        .filter(|value| model::truthy(Some(value)))
        .cloned()
        .ok_or_else(|| {
            FlowError::progress(
                "batch publication requires a verified complete trusted gate result and released writer",
            )
        })
}

fn publication_target(
    s: &J,
    t: &J,
    now: f64,
    api: Option<&dyn GitHubApi>,
    lane: &J,
    batch: &J,
) -> Result<()> {
    crate::batch::actions::check(lane)?;
    if crate::batch::actions::enabled(lane) {
        crate::batch::actions::publication(lane, batch)?;
    } else {
        ensure_publication_ready(t, batch)?;
    }
    let fallback = crate::ci::GitHub::new(lane.clone());
    let api: &dyn GitHubApi = api.unwrap_or(&fallback);
    publication_dependencies(s, t, now, lane, batch, api)
}

fn ensure_publication_ready(t: &J, batch: &J) -> Result<()> {
    if publication_ready(t, batch) {
        Ok(())
    } else {
        Err(FlowError::progress(
            "batch publication requires a verified complete trusted gate result and released writer",
        ))
    }
}

fn publication_ready(t: &J, batch: &J) -> bool {
    [
        model::string(batch, "phase").as_deref() == Some("gate_passed"),
        !model::truthy(model::get(t, "writer")),
    ]
    .into_iter()
    .all(|ok| ok)
}

fn publication_dependencies(
    s: &J,
    t: &J,
    now: f64,
    lane: &J,
    batch: &J,
    api: &dyn GitHubApi,
) -> Result<()> {
    dependencies(s, lane, batch, api)?;
    publication_latest(t, now, lane, batch, api)
}

fn publication_latest(t: &J, now: f64, lane: &J, batch: &J, api: &dyn GitHubApi) -> Result<()> {
    let latest = latest(lane, api)?;
    if crate::batch::actions::enabled(lane) {
        crate::batch::actions::publication_settings(t, api)?;
    }
    publication_after_target(t, now, batch, &latest, lane)
}

fn publication_after_target(t: &J, now: f64, batch: &J, latest: &str, lane: &J) -> Result<()> {
    if latest != model::string(batch, "base").unwrap_or_default() {
        return Err(FlowError::progress(
            "target changed before publication; rebuild and revalidate",
        ));
    }
    if crate::batch::actions::enabled(lane) {
        return publication_pr(t, batch);
    }
    publication_gate(t, now, lane, batch)
}

fn publication_gate(t: &J, now: f64, lane: &J, batch: &J) -> Result<()> {
    let receipt = publication_receipt(lane, batch, now)?;
    publication_receipt_check(t, batch, &receipt)
}

fn publication_receipt(lane: &J, batch: &J, now: f64) -> Result<J> {
    let reference = model::dig(batch, &["gate", "reference"]).cloned();
    gate::query(lane, batch, reference.as_ref(), now)
}

fn publication_receipt_check(t: &J, batch: &J, receipt: &J) -> Result<()> {
    if publication_receipt_ok(batch, receipt) {
        publication_pr(t, batch)
    } else {
        Err(FlowError::progress(
            "trusted gate result changed or was revoked before publication",
        ))
    }
}

fn publication_receipt_ok(batch: &J, receipt: &J) -> bool {
    [receipt_allows(receipt), receipt_stored(batch, receipt)]
        .into_iter()
        .all(|ok| ok)
}

fn receipt_allows(receipt: &J) -> bool {
    model::string(receipt, "decision").as_deref() == Some("allow")
}

fn receipt_stored(batch: &J, receipt: &J) -> bool {
    model::get(batch, "gate").and_then(|gate| gate.get("receipt")) == Some(receipt)
}

fn publication_pr(t: &J, batch: &J) -> Result<()> {
    if publication_pr_ok(t, batch) {
        Ok(())
    } else {
        Err(FlowError::progress(
            "PR no longer matches the validated batch",
        ))
    }
}

fn publication_pr_ok(t: &J, batch: &J) -> bool {
    [publication_head_ok(t, batch), publication_base_ok(t, batch)]
        .into_iter()
        .all(|ok| ok)
}

fn publication_head_ok(t: &J, batch: &J) -> bool {
    model::dig_str(t, &["pr", "head"]) == model::dig_str(batch, &["snapshot", "head"])
}

fn publication_base_ok(t: &J, batch: &J) -> bool {
    model::dig_str(t, &["pr", "target_base"]) == model::string(batch, "base")
}
