//! Review configuration, attestation and non-replayed recovery journal.

use super::{fingerprint, guard_error, read_state, save, transcript_state, verify_handoff};
use crate::error::FlowError;
use crate::error::Result;
use crate::herdr::GuardAdapter;
use crate::model;
use crate::model::J;
use crate::util;
use serde_json::json;
use serde_json::Value;
use std::path::Path;
use std::thread;
use std::time::Duration;
use std::time::Instant;

pub fn review_events(root: &Path, s: &J) -> Result<J> {
    util::ensure_private_dir(root, false)?;
    let path = util::resolve_path(root).join("review-events.json");
    if !path.exists() {
        return Ok(review_events_absent());
    }
    review_events_read(&path, s)
}

fn review_events_absent() -> J {
    json!({"status": "not_observed", "events": []})
}

fn review_events_stale() -> J {
    json!({"status": "stale", "events": []})
}

fn review_events_read(path: &Path, s: &J) -> Result<J> {
    let journal = util::read_json(path)?;
    review_events_checked(journal, s)
}

fn review_events_checked(journal: J, s: &J) -> Result<J> {
    if !review_events_valid(&journal) {
        return Err(guard_error("invalid automatic review event snapshot"));
    }
    review_events_identity(journal, s)
}

fn review_events_valid(journal: &J) -> bool {
    [
        model::get(journal, "version").and_then(Value::as_i64) == Some(1),
        model::get(journal, "events")
            .and_then(Value::as_array)
            .is_some(),
        model::get(journal, "overflow_count")
            .and_then(Value::as_i64)
            .is_some(),
    ]
    .into_iter()
    .all(|valid| valid)
}

fn review_events_identity(journal: J, s: &J) -> Result<J> {
    if review_events_is_stale(&journal, s) {
        return Ok(review_events_stale());
    }
    Ok(review_events_observed(journal))
}

fn review_events_is_stale(journal: &J, s: &J) -> bool {
    let expected_thread = review_events_expected_thread(s);
    [
        model::get(s, "bridge_nonce").is_none(),
        model::string(journal, "nonce") != model::string(s, "bridge_nonce"),
        model::get(journal, "thread_id") != Some(&expected_thread),
    ]
    .into_iter()
    .any(|stale| stale)
}

fn review_events_expected_thread(s: &J) -> J {
    model::get(s, "successor")
        .cloned()
        .or_else(|| model::get(s, "session").cloned())
        .unwrap_or(Value::Null)
}

fn review_events_observed(mut journal: J) -> J {
    journal["status"] = json!("observed");
    journal
}

fn review_state(root: &Path) -> Result<(J, J)> {
    let s = read_state(root)?;
    review_state_checked(s)
}

fn review_state_checked(s: J) -> Result<(J, J)> {
    if review_state_disallowed(&s) {
        return Err(guard_error(
            "disable a settled guard and acknowledge deliveries before configuring review",
        ));
    }
    review_state_transcript(s)
}

fn review_state_disallowed(s: &J) -> bool {
    [
        model::int(s, "version") != Some(2),
        !matches!(
            model::string(s, "phase").as_deref(),
            Some("monitoring" | "parked")
        ),
        model::bool_or(s, "auto_rotate", false),
        external_delivery_pending(s),
    ]
    .into_iter()
    .any(|disallowed| disallowed)
}

fn review_state_transcript(s: J) -> Result<(J, J)> {
    let transcript = model::string(&s, "transcript")
        .ok_or_else(|| guard_error("review configuration transcript identity mismatch"))?;
    review_state_data(s, &transcript)
}

fn review_state_data(s: J, transcript: &str) -> Result<(J, J)> {
    let data = transcript_state(Path::new(transcript), None)?;
    review_state_identity(s, data)
}

fn review_state_identity(s: J, data: J) -> Result<(J, J)> {
    if review_state_identity_mismatch(&s, &data) {
        return Err(guard_error(
            "review configuration transcript identity mismatch",
        ));
    }
    Ok((s, data))
}

fn review_state_identity_mismatch(s: &J, data: &J) -> bool {
    [
        model::string(data, "session") != model::string(s, "session"),
        model::string(data, "cwd") != model::string(s, "cwd"),
    ]
    .into_iter()
    .any(|mismatch| mismatch)
}

pub(super) fn external_delivery_pending(s: &J) -> bool {
    model::get(s, "external_delivery")
        .map(|value| model::truthy(Some(value)))
        .unwrap_or(false)
}

fn review_attestation(s: &J, handoff: Option<&Path>, data: &J) -> Result<()> {
    if model::int(data, "user_messages").unwrap_or(0) == 0 {
        return Ok(());
    }
    let Some(handoff) = handoff else {
        return Err(guard_error(
            "an existing work session requires a quiescent --handoff attestation",
        ));
    };
    verify_handoff(s, handoff)?;
    Ok(())
}

fn review_adapter(h: &mut dyn GuardAdapter, require_bridge: bool) -> Result<J> {
    if require_bridge {
        return review_adapter_bridge(h);
    }
    review_adapter_idle(h)
}

fn review_adapter_bridge(h: &mut dyn GuardAdapter) -> Result<J> {
    let marker = h.marker()?;
    review_adapter_marker(h, marker)
}

fn review_adapter_marker(h: &mut dyn GuardAdapter, marker: Option<J>) -> Result<J> {
    if !review_adapter_ready(marker.as_ref()) {
        return Err(guard_error(
            "reattach the quiescent native TUI through the updated bridge before enabling review",
        ));
    }
    review_adapter_idle(h)
}

fn review_adapter_ready(marker: Option<&J>) -> bool {
    marker.and_then(|marker| model::int(marker, "review_events_version")) == Some(1)
}

fn review_adapter_idle(h: &mut dyn GuardAdapter) -> Result<J> {
    let idle = h.idle()?;
    review_adapter_idle_check(h, idle)
}

fn review_adapter_idle_check(h: &mut dyn GuardAdapter, idle: bool) -> Result<J> {
    if !idle {
        return Err(guard_error(
            "review configuration requires an idle registered native thread",
        ));
    }
    h.snapshot(None)
}

fn apply_review(root: &Path, s: &mut J, h: &mut dyn GuardAdapter, before: &J) -> Result<J> {
    let expected = crate::herdr::auto_review_runtime(before)?;
    let checkout = fingerprint(&model::string(s, "cwd").unwrap_or_default())?;
    let change = json!({
        "session": s.get("session").cloned().unwrap_or(Value::Null),
        "before": before,
        "expected": expected,
        "previous_phase": s.get("phase").cloned().unwrap_or(Value::Null),
        "checkout": checkout,
        "at": util::now()
    });
    s["review_change"] = change.clone();
    s["phase"] = json!("review_config_sending");
    save(root, s)?;
    h.set_state(s);
    let result = apply_review_settings(s, h, &expected, &change);
    match result {
        Ok(actual) => {
            s["phase"] = change
                .get("previous_phase")
                .cloned()
                .unwrap_or_else(|| json!("monitoring"));
            s["review_policy"] = actual;
            s["review_change"]["status"] = json!("verified");
            save(root, s)?;
            Ok(s.clone())
        }
        Err(error) => {
            s["phase"] = json!("needs_attention");
            s["phase_before_error"] = json!("review_config_sending");
            s["error"] = json!(error.to_string());
            save(root, s)?;
            Err(guard_error(
                "review configuration unconfirmed; inspect then review-reconcile, never replay",
            ))
        }
    }
}

fn apply_review_settings(s: &J, h: &mut dyn GuardAdapter, expected: &J, change: &J) -> Result<J> {
    h.rpc(
        "thread/settings/update",
        &json!({
            "threadId":s.get("session").cloned().unwrap_or(Value::Null),
            "approvalPolicy":"on-request", "approvalsReviewer":"auto_review",
            "sandboxPolicy":expected.get("sandbox").cloned().unwrap_or(Value::Null)
        }),
    )?;
    await_review_settings(h, expected)?;
    let actual = h.snapshot(None)?;
    if fingerprint(&model::string(s, "cwd").unwrap_or_default())?
        != change["checkout"].as_str().unwrap_or_default()
    {
        return Err(guard_error("checkout changed during review configuration"));
    }
    Ok(actual)
}

fn await_review_settings(h: &mut dyn GuardAdapter, expected: &J) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let actual = h.snapshot(None)?;
        match crate::herdr::verify_review_runtime(expected, &actual) {
            Ok(()) => return Ok(()),
            Err(error) => {
                if Instant::now() >= deadline {
                    return Err(error);
                }
                thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

pub fn configure_review(
    root: &Path,
    handoff: Option<&Path>,
    h: &mut dyn GuardAdapter,
    require_bridge: bool,
) -> Result<J> {
    let (mut s, data) = review_state(root)?;
    review_attestation(&s, handoff, &data)?;
    h.set_state(&s);
    let before = review_adapter(h, require_bridge)?;
    apply_review(root, &mut s, h, &before)
}

pub fn reconcile_review(root: &Path, h: &mut dyn GuardAdapter) -> Result<J> {
    let s = read_state(root)?;
    reconcile_review_state(root, s, h)
}

fn reconcile_review_state(root: &Path, s: J, h: &mut dyn GuardAdapter) -> Result<J> {
    reconcile_review_phase(&s)?;
    reconcile_review_live(root, s, h)
}

fn reconcile_review_phase(s: &J) -> Result<()> {
    if reconcile_review_phase_ready(s) {
        return Ok(());
    }
    Err(guard_error(
        "no ambiguous review configuration to reconcile",
    ))
}

fn reconcile_review_phase_ready(s: &J) -> bool {
    let phase = model::string(s, "phase").unwrap_or_default();
    let before = model::string(s, "phase_before_error").unwrap_or_default();
    [phase].into_iter().any(|phase| {
        phase == "review_config_sending"
            || (phase == "needs_attention" && before == "review_config_sending")
    })
}

fn reconcile_review_live(root: &Path, s: J, h: &mut dyn GuardAdapter) -> Result<J> {
    let change = reconcile_review_change(&s)?;
    reconcile_review_identity(root, s, h, change)
}

fn reconcile_review_change(s: &J) -> Result<J> {
    model::get(s, "review_change")
        .cloned()
        .ok_or_else(|| guard_error("review configuration journal is unavailable"))
}

fn reconcile_review_identity(root: &Path, s: J, h: &mut dyn GuardAdapter, change: J) -> Result<J> {
    if model::get(&s, "session") != model::get(&change, "session") {
        return Err(guard_error(
            "review configuration session or checkout changed",
        ));
    }
    reconcile_review_checkout(root, s, h, change)
}

fn reconcile_review_checkout(root: &Path, s: J, h: &mut dyn GuardAdapter, change: J) -> Result<J> {
    let checkout = reconcile_review_checkout_value(&s)?;
    reconcile_review_checkout_compare(root, s, h, change, checkout)
}

fn reconcile_review_checkout_compare(
    root: &Path,
    s: J,
    h: &mut dyn GuardAdapter,
    change: J,
    checkout: String,
) -> Result<J> {
    if checkout != model::string(&change, "checkout").unwrap_or_default() {
        return Err(guard_error(
            "review configuration session or checkout changed",
        ));
    }
    reconcile_review_apply(root, s, h, change)
}

fn reconcile_review_checkout_value(s: &J) -> Result<String> {
    fingerprint(&model::string(s, "cwd").unwrap_or_default())
}

fn reconcile_review_apply(root: &Path, mut s: J, h: &mut dyn GuardAdapter, change: J) -> Result<J> {
    h.set_state(&s);
    let actual = reconcile_review_actual(h)?;
    reconcile_review_stored(root, &mut s, &change, &actual)
}

fn reconcile_review_stored(root: &Path, s: &mut J, change: &J, actual: &J) -> Result<J> {
    let status = reconcile_review_status(s, change, actual)?;
    reconcile_review_persist(root, s, change, status)
}

fn reconcile_review_actual(h: &mut dyn GuardAdapter) -> Result<J> {
    let idle = h.idle()?;
    reconcile_review_actual_check(h, idle)
}

fn reconcile_review_actual_check(h: &mut dyn GuardAdapter, idle: bool) -> Result<J> {
    if !idle {
        return Err(guard_error(
            "inspect after the registered thread becomes idle",
        ));
    }
    h.snapshot(None)
}

fn reconcile_review_status(s: &mut J, change: &J, actual: &J) -> Result<&'static str> {
    crate::herdr::verify_review_runtime(change.get("expected").unwrap_or(&Value::Null), actual)
        .map(|()| {
            s["review_policy"] = actual.clone();
            "verified"
        })
        .or_else(|error| reconcile_review_not_applied(change, actual, error))
}

fn reconcile_review_not_applied(change: &J, actual: &J, error: FlowError) -> Result<&'static str> {
    let before = change.get("before").cloned().unwrap_or_else(|| json!({}));
    reconcile_review_previous(&before, actual, &error)?;
    Ok("not_applied")
}

fn reconcile_review_previous(before: &J, actual: &J, error: &FlowError) -> Result<()> {
    crate::herdr::same_runtime(before, actual).map_err(|_| {
        guard_error(format!(
            "review configuration differs from both expected and previous settings: {error}"
        ))
    })
}

fn reconcile_review_persist(root: &Path, s: &mut J, change: &J, status: &str) -> Result<J> {
    reconcile_review_history(root, s)?;
    reconcile_review_finish(root, s, change, status)
}

fn reconcile_review_history(root: &Path, s: &J) -> Result<()> {
    let history = util::resolve_path(root)
        .join("history")
        .join(format!("review-recovery-{}.json", util::now_ns()));
    util::atomic_write_json(&history, s)
}

fn reconcile_review_finish(root: &Path, s: &mut J, change: &J, status: &str) -> Result<J> {
    s["review_change"]["status"] = json!(status);
    s["phase"] = change
        .get("previous_phase")
        .cloned()
        .unwrap_or_else(|| json!("monitoring"));
    s["auto_rotate"] = json!(false);
    model::remove(s, "error");
    model::remove(s, "phase_before_error");
    save(root, s)?;
    Ok(s.clone())
}
