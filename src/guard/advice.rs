//! Optional advisor binding, stale evidence checks and bounded waits.

use super::{
    external_delivery_pending, fingerprint, guard_error, handoff_digest, read_state, save,
};
use crate::error::FlowError;
use crate::error::Result;
use crate::model;
use crate::model::J;
use crate::util;
use serde_json::json;
use serde_json::Map;
use serde_json::Value;
use std::path::Path;
use std::path::PathBuf;
use std::thread;
use std::time::Duration;
use std::time::Instant;

pub fn advise(root: &Path, path: &Path, session: &str) -> Result<J> {
    let _permit = crate::lifecycle::runtime_permit(root)?;
    let s = read_state(root)?;
    advise_state(root, path, session, s)
}

fn advise_state(root: &Path, path: &Path, session: &str, s: J) -> Result<J> {
    if advise_session_invalid(&s, session) {
        return Err(guard_error(
            "advice requires the registered app-server session",
        ));
    }
    advise_settled(root, path, s)
}

fn advise_session_invalid(s: &J, session: &str) -> bool {
    [
        model::int(s, "version") != Some(2),
        model::string(s, "session").as_deref() != Some(session),
    ]
    .into_iter()
    .any(|invalid| invalid)
}

fn advise_settled(root: &Path, path: &Path, s: J) -> Result<J> {
    if advise_phase_invalid(&s) {
        return Err(guard_error(
            "advice requires a settled guard without pending delivery",
        ));
    }
    advise_enabled(root, path, s)
}

fn advise_phase_invalid(s: &J) -> bool {
    let phase = model::string(s, "phase").unwrap_or_default();
    [
        !matches!(phase.as_str(), "monitoring" | "requested" | "parked"),
        external_delivery_pending(s),
    ]
    .into_iter()
    .any(|invalid| invalid)
}

fn advise_enabled(root: &Path, path: &Path, s: J) -> Result<J> {
    if !model::bool_or(&s, "advisor_enabled", true) {
        return Ok(json!({"status": "disabled"}));
    }
    advise_read(root, path, s)
}

fn advise_read(root: &Path, path: &Path, s: J) -> Result<J> {
    let raw = util::read_bytes(path)?;
    advise_raw(root, path, s, raw)
}

fn advise_raw(root: &Path, path: &Path, s: J, raw: Vec<u8>) -> Result<J> {
    if raw.len() > 16000 {
        return Err(guard_error("handoff exceeds 16 KB"));
    }
    advise_parse(root, path, s, raw)
}

fn advise_parse(root: &Path, path: &Path, s: J, raw: Vec<u8>) -> Result<J> {
    let handoff: J = serde_json::from_slice(&raw)?;
    advise_validate(root, path, s, handoff)
}

fn advise_validate(root: &Path, path: &Path, s: J, handoff: J) -> Result<J> {
    if advise_draft_invalid(&s, &handoff) {
        return Err(guard_error("draft task/epoch/cwd mismatch"));
    }
    advise_bind(root, path, s, handoff)
}

fn advise_draft_invalid(s: &J, handoff: &J) -> bool {
    [
        !handoff.is_object(),
        ["task", "epoch", "cwd"]
            .iter()
            .any(|key| model::get(handoff, key) != model::get(s, key)),
    ]
    .into_iter()
    .any(|invalid| invalid)
}

fn advise_bind(root: &Path, path: &Path, mut s: J, handoff: J) -> Result<J> {
    let cwd = model::string(&s, "cwd").unwrap_or_default();
    let checkout = fingerprint(&cwd)?;
    let binding = advise_binding(&s, checkout, &handoff);
    let job = advise_job(root, &binding, &handoff);
    let reference = advise_reference(&job, binding, path);
    s["advisor"] = reference;
    advise_save(root, s)
}

fn advise_binding(s: &J, checkout: String, handoff: &J) -> J {
    json!({
        "task": s.get("task").cloned().unwrap_or(Value::Null),
        "epoch": s.get("epoch").cloned().unwrap_or(Value::Null),
        "session": s.get("session").cloned().unwrap_or(Value::Null),
        "role": s.get("role").cloned().unwrap_or(Value::Null),
        "cwd": s.get("cwd").cloned().unwrap_or(Value::Null),
        "app_server_socket": s.get("app_server_socket").cloned().unwrap_or(Value::Null),
        "checkout": checkout,
        "handoff_sha": handoff_digest(handoff)
    })
}

fn advise_job(root: &Path, binding: &J, handoff: &J) -> J {
    crate::codex::advisor::request(root, binding, handoff)
        .unwrap_or_else(|error| json!({"status": "error", "error": error.to_string()}))
}

fn advise_reference(job: &J, binding: J, path: &Path) -> J {
    let mut reference = advise_reference_fields(job);
    reference.insert("binding".to_string(), binding);
    reference.insert(
        "source".to_string(),
        json!(util::resolve_path(path).display().to_string()),
    );
    Value::Object(reference)
}

fn advise_reference_fields(job: &J) -> Map<String, Value> {
    ["id", "path", "result_path", "status", "error"]
        .iter()
        .filter_map(|key| model::get(job, key).map(|value| (key.to_string(), value.clone())))
        .collect()
}

fn advise_save(root: &Path, mut s: J) -> Result<J> {
    save(root, &mut s)?;
    advice_status(root, &s)
}

pub fn advice_status(_root: &Path, s: &J) -> Result<J> {
    let Some(reference) = advice_reference(s) else {
        return Ok(advice_absent(s));
    };
    advice_present(s, &reference)
}

fn advice_reference(s: &J) -> Option<J> {
    model::get(s, "advisor")
        .filter(|value| model::truthy(Some(value)))
        .cloned()
}

fn advice_absent(s: &J) -> J {
    json!({"status": if model::bool_or(s, "advisor_enabled", true) { "not_requested" } else { "disabled" }})
}

fn advice_present(s: &J, reference: &J) -> Result<J> {
    let mut result = advice_base(reference);
    let outcome = advice_enrich(s, reference, &mut result);
    Ok(outcome.map_or_else(|error| advice_unavailable(reference, &error), |()| result))
}

fn advice_base(reference: &J) -> J {
    let result: Map<String, Value> = reference
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(key, _)| key.as_str() != "binding" && key.as_str() != "source")
        .map(|(key, value)| (key.to_string(), value.clone()))
        .collect();
    Value::Object(result)
}

fn advice_unavailable(reference: &J, error: &FlowError) -> J {
    json!({
        "status": "unavailable",
        "id": reference.get("id").cloned().unwrap_or(Value::Null),
        "error": error.to_string()
    })
}

fn advice_enrich(s: &J, reference: &J, result: &mut J) -> Result<()> {
    advice_enrich_result_file(reference, result)?;
    advice_verify(reference, s, result)
}

fn advice_enrich_result_file(reference: &J, result: &mut J) -> Result<()> {
    let Some(path) = model::string(reference, "result_path") else {
        return Ok(());
    };
    advice_merge_result_file(&path, result)
}

fn advice_merge_result_file(path: &str, result: &mut J) -> Result<()> {
    if !Path::new(path).exists() {
        return Ok(());
    }
    advice_merge_json(path, result)
}

fn advice_merge_json(path: &str, result: &mut J) -> Result<()> {
    let extra = util::read_json(Path::new(path))?;
    advice_merge_extra(extra, result);
    Ok(())
}

fn advice_merge_extra(extra: J, result: &mut J) {
    extra
        .as_object()
        .into_iter()
        .flatten()
        .for_each(|(key, value)| {
            result[key] = value.clone();
        });
}

fn advice_verify(reference: &J, s: &J, result: &mut J) -> Result<()> {
    let binding = advice_binding(reference)?;
    advice_verify_current(reference, &binding, s, result)
}

fn advice_binding(reference: &J) -> Result<J> {
    model::get(reference, "binding")
        .cloned()
        .ok_or_else(|| guard_error("advisor binding missing"))
}

fn advice_verify_current(reference: &J, binding: &J, s: &J, result: &mut J) -> Result<()> {
    if advice_fields_mismatch(s, binding) {
        return advice_mark_stale(reference, result);
    }
    advice_verify_checkout(reference, binding, s, result)
}

fn advice_fields_mismatch(s: &J, binding: &J) -> bool {
    [
        "task",
        "epoch",
        "session",
        "role",
        "cwd",
        "app_server_socket",
    ]
    .iter()
    .any(|key| model::get(s, key) != model::get(binding, key))
}

fn advice_verify_checkout(reference: &J, binding: &J, s: &J, result: &mut J) -> Result<()> {
    let current = fingerprint(&model::string(s, "cwd").unwrap_or_default())?;
    advice_compare_checkout(reference, binding, current, result)
}

fn advice_compare_checkout(
    reference: &J,
    binding: &J,
    current: String,
    result: &mut J,
) -> Result<()> {
    if current != model::string(binding, "checkout").unwrap_or_default() {
        return advice_mark_stale(reference, result);
    }
    advice_verify_source(reference, binding, result)
}

fn advice_verify_source(reference: &J, binding: &J, result: &mut J) -> Result<()> {
    let current = advice_source_digest(reference)?;
    advice_compare_source(reference, binding, current, result)
}

fn advice_source_digest(reference: &J) -> Result<String> {
    let source = PathBuf::from(model::string(reference, "source").unwrap_or_default());
    Ok(handoff_digest(&util::read_json(&source)?))
}

fn advice_compare_source(
    reference: &J,
    binding: &J,
    current: String,
    result: &mut J,
) -> Result<()> {
    if current != model::string(binding, "handoff_sha").unwrap_or_default() {
        return advice_mark_stale(reference, result);
    }
    Ok(())
}

fn advice_mark_stale(reference: &J, result: &mut J) -> Result<()> {
    *result = json!({
        "status": "stale",
        "id": reference.get("id").cloned().unwrap_or(Value::Null),
        "result_path": reference.get("result_path").cloned().unwrap_or(Value::Null)
    });
    Ok(())
}

pub fn wait_advice(root: &Path, seconds: f64) -> Result<J> {
    let deadline = Instant::now() + Duration::from_secs_f64(seconds);
    wait_advice_until(root, deadline)
}

fn wait_advice_until(root: &Path, deadline: Instant) -> Result<J> {
    let mut result = None;
    (|| -> Result<()> {
        while result.is_none() {
            result = wait_advice_step(root, deadline)?;
        }
        Ok(())
    })()?;
    Ok(result.unwrap_or(Value::Null))
}

fn wait_advice_step(root: &Path, deadline: Instant) -> Result<Option<J>> {
    let state = read_state(root)?;
    wait_advice_status(root, deadline, &state)
}

fn wait_advice_status(root: &Path, deadline: Instant, state: &J) -> Result<Option<J>> {
    let result = advice_status(root, state)?;
    wait_advice_finish(result, deadline)
}

fn wait_advice_finish(result: J, deadline: Instant) -> Result<Option<J>> {
    if wait_advice_keep_waiting(&result, deadline) {
        wait_advice_sleep(deadline);
        return Ok(None);
    }
    Ok(Some(result))
}

fn wait_advice_keep_waiting(result: &J, deadline: Instant) -> bool {
    [wait_advice_pending(result), Instant::now() < deadline]
        .into_iter()
        .all(|waiting| waiting)
}

fn wait_advice_pending(result: &J) -> bool {
    matches!(
        model::string(result, "status").as_deref(),
        Some("queued" | "running")
    )
}

fn wait_advice_sleep(deadline: Instant) {
    let remaining = deadline.saturating_duration_since(Instant::now());
    thread::sleep(remaining.min(Duration::from_millis(200)));
}
