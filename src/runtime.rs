//! Current receiver capability checks; no worker dispatch or permission changes.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::error::{ErrorKind, FlowError, Result};
use crate::guard;
use crate::herdr::ReceiverFactory;
use crate::model::{self, J};
use crate::progress;
use crate::util::FileLock;
use crate::watch_service;

pub mod role;

pub fn scope(g: &J) -> J {
    guard::scope(g)
}

fn readiness_report(
    root: &Path,
    s: &J,
    session: Option<&str>,
) -> Result<(J, Vec<String>, Vec<String>)> {
    let gaps = Vec::new();
    let next_steps = Vec::new();
    let producer_channels = producer_channels(root, s)?;
    let last_confirmed = last_confirmed_receipt(s, session);
    let result = json!({
        "queue": s.get("id").cloned().unwrap_or(Value::Null),
        "checked_at": crate::util::now(),
        "session": session,
        "result_return": false,
        "active_wakeup": false,
        "context_telemetry": false,
        "safe_rotation": false,
        "role_context": "unknown",
        "gaps": gaps.clone(),
        "next_steps": next_steps.clone(),
        "producer_channels": producer_channels,
        "last_confirmed_receipt": last_confirmed
    });
    Ok((result, gaps, next_steps))
}

fn producer_channels(root: &Path, s: &J) -> Result<Vec<J>> {
    let mut producer_channels = Vec::new();
    if let Some(waits) = model::get(s, "waits").and_then(Value::as_object) {
        for w in waits.values() {
            if model::bool_or(w, "closed", false) {
                continue;
            }
            let wait_id = model::string(w, "id").unwrap_or_default();
            let kind = model::string(w, "kind").unwrap_or_default();
            let result_path = if kind == "signal" {
                Some(
                    crate::queue::artifacts(root)?
                        .join("results")
                        .join(format!("{wait_id}.json"))
                        .display()
                        .to_string(),
                )
            } else {
                model::string(w, "source")
            };
            let binding_value = if model::get(w, "assignment").is_some() {
                Some(progress::binding(w)?)
            } else {
                None
            };
            producer_channels.push(json!({
                "wait": wait_id,
                "kind": kind,
                "owner": w.get("owner").cloned().unwrap_or(Value::Null),
                "session": crate::identity::producer(w)?.map(|id|id["session"].clone()),
                "producer": crate::identity::producer(w)?,
                "result_path": result_path,
                "binding": binding_value
            }));
        }
    }
    Ok(producer_channels)
}

fn last_confirmed_receipt(s: &J, session: Option<&str>) -> Option<J> {
    let mut last_confirmed = None;
    if let Some(deliveries) = model::get(s, "deliveries").and_then(Value::as_object) {
        let mut candidates: Vec<&J> = deliveries
            .values()
            .filter(|delivery| delivery_acknowledged_by(delivery, session))
            .collect();
        candidates.sort_by(|left, right| {
            model::number(left, "ack_at")
                .unwrap_or(0.0)
                .partial_cmp(&model::number(right, "ack_at").unwrap_or(0.0))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        if let Some(delivery) = candidates.last() {
            last_confirmed = Some(json!({
                "delivery": delivery.get("id").cloned().unwrap_or(Value::Null),
                "session": delivery.get("session").cloned().unwrap_or(Value::Null),
                "at": delivery.get("ack_at").cloned().unwrap_or(Value::Null)
            }));
        }
    }
    last_confirmed
}

fn delivery_acknowledged_by(delivery: &J, session: Option<&str>) -> bool {
    model::string(delivery, "phase").as_deref() == Some("acknowledged")
        && model::string(delivery, "session").as_deref() == session
}

fn verify_receiver(
    s: &J,
    session: &str,
    factory: &ReceiverFactory,
    result: &mut J,
    gaps: &mut Vec<String>,
    next_steps: &mut Vec<String>,
) -> Result<()> {
    let (state, _lock, telemetry) = receiver_checked(s, session, factory)?;
    record_receiver(s, &state, telemetry, result, gaps, next_steps);
    Ok(())
}

fn ensure(condition: bool, message: &str) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(FlowError::progress(message))
    }
}

fn guard_path(s: &J) -> PathBuf {
    let guard_dir = model::string(s, "guard_dir").unwrap_or_default();
    PathBuf::from(guard_dir)
}

fn lock_guard(path: &Path) -> Result<FileLock> {
    guard::locked(path)
        .map_err(|error| FlowError::progress(format!("guard state is busy: {error}")))
}

fn read_guard_state(path: &Path) -> Result<J> {
    crate::util::read_state_json(path, "state.json")
}

fn guard_scope_matches(s: &J, state: &J) -> bool {
    guard::scope(state) == model::get(s, "receiver").cloned().unwrap_or(Value::Null)
}

fn guard_session_matches(state: &J, session: &str) -> bool {
    model::string(state, "session").as_deref() == Some(session)
}

fn guard_identity_matches(s: &J, session: &str, state: &J) -> bool {
    guard_scope_matches(s, state) && guard_session_matches(state, session)
}

fn ensure_guard_identity(s: &J, session: &str, state: &J) -> Result<()> {
    ensure(
        guard_identity_matches(s, session, state),
        "guard belongs to another registration/session; explicit rebind required",
    )
}

fn watcher_healthy(path: &Path) -> bool {
    model::bool_or(&watch_service::health(path), "healthy", false)
}

fn ensure_watcher_healthy(path: &Path) -> Result<()> {
    ensure(
        watcher_healthy(path),
        "receiver watcher is not healthy; inspect context_guard status and restart only the verified watcher",
    )
}

fn ensure_agent_session(agent: &J, session: &str) -> Result<()> {
    ensure(
        model::dig_str(agent, &["agent_session", "value"]).as_deref() == Some(session),
        "app-server does not confirm the exact receiver session; inspect the live pane/session before sending",
    )
}

fn usage_nonempty(usage: &serde_json::Map<String, Value>) -> bool {
    !usage.is_empty()
}

fn usage_tokens(usage: &serde_json::Map<String, Value>) -> bool {
    usage.get("tokens").and_then(Value::as_i64).is_some()
}

fn usage_window(usage: &serde_json::Map<String, Value>) -> bool {
    usage
        .get("window")
        .and_then(Value::as_i64)
        .map(|window| window > 0)
        .unwrap_or(false)
}

fn usage_measured(usage: &serde_json::Map<String, Value>) -> bool {
    usage_nonempty(usage) && usage_tokens(usage)
}

fn usage_windowed(usage: &serde_json::Map<String, Value>) -> bool {
    usage_measured(usage) && usage_window(usage)
}

fn level_known(state: &J) -> bool {
    model::string(state, "level").as_deref() != Some("unknown")
}

fn context_telemetry(state: &J) -> bool {
    if state.get("adapter").is_some_and(|value| value != "codex") {
        return false;
    }
    model::get(state, "usage")
        .and_then(Value::as_object)
        .map(|usage| usage_windowed(usage) && level_known(state))
        .unwrap_or(false)
}

fn receiver_guard(s: &J, session: &str) -> Result<(J, FileLock)> {
    let path = guard_path(s);
    lock_guard(&path)
        .and_then(|lock| read_guard_state(&path).map(|state| (state, lock)))
        .and_then(|(state, lock)| ensure_guard_identity(s, session, &state).map(|()| (state, lock)))
        .and_then(|(state, lock)| ensure_watcher_healthy(&path).map(|()| (state, lock)))
}

fn receiver_telemetry(state: &J, session: &str, factory: &ReceiverFactory) -> Result<bool> {
    factory(state)
        .and_then(|adapter| adapter.agent())
        .and_then(|agent| ensure_agent_session(&agent, session).map(|()| context_telemetry(state)))
}

fn receiver_checked(
    s: &J,
    session: &str,
    factory: &ReceiverFactory,
) -> Result<(J, FileLock, bool)> {
    receiver_guard(s, session).and_then(|(state, lock)| {
        receiver_telemetry(&state, session, factory).map(|telemetry| (state, lock, telemetry))
    })
}

fn phase_not_monitoring(state: &J) -> bool {
    model::string(state, "phase").as_deref() != Some("monitoring")
}

fn rotation_due(state: &J) -> bool {
    model::bool_or(state, "rotation_due", false)
}

fn external_delivery(state: &J) -> bool {
    model::get(state, "external_delivery")
        .map(|value| model::truthy(Some(value)))
        .unwrap_or(false)
}

fn queue_active(s: &J) -> bool {
    model::get(s, "active")
        .map(|value| model::truthy(Some(value)))
        .unwrap_or(false)
}

fn handoff_phase(state: &J) -> bool {
    phase_not_monitoring(state) || rotation_due(state)
}

fn handoff_state(state: &J) -> bool {
    handoff_phase(state) || external_delivery(state)
}

fn needs_handoff(state: &J, s: &J) -> bool {
    handoff_state(state) || queue_active(s)
}

fn rotation_ready(gaps: &[String], telemetry: bool) -> bool {
    gaps.is_empty() && telemetry
}

fn safe_rotation(gaps: &[String], telemetry: bool, state: &J) -> bool {
    rotation_ready(gaps, telemetry) && model::bool_or(state, "auto_rotate", false)
}

fn push_handoff_gap(s: &J, state: &J, gaps: &mut Vec<String>, next_steps: &mut Vec<String>) {
    if needs_handoff(state, s) {
        gaps.push("receiver is handing off, awaiting receipt, or needs attention".to_string());
        next_steps.push(
            "ACK or resolve the existing delivery, or finish the safe handoff checkpoint first"
                .to_string(),
        );
    }
}

fn push_telemetry_gap(gaps: &mut Vec<String>, next_steps: &mut Vec<String>, telemetry: bool) {
    if !telemetry {
        gaps.push("context telemetry unknown; inspect guard observation".to_string());
        next_steps.push(
            "inspect the transcript identity and guard observation; do not claim automatic protection"
                .to_string(),
        );
    }
}

fn push_rotation_gap(gaps: &mut Vec<String>, next_steps: &mut Vec<String>, state: &J) {
    if !model::bool_or(state, "auto_rotate", false) {
        gaps.push("automatic rotation is not enabled".to_string());
        next_steps.push(
            "if automatic rotation is required and authorized, enable and verify it; otherwise record that rotation is unsupported"
                .to_string(),
        );
    }
}

fn record_receiver(
    s: &J,
    state: &J,
    telemetry: bool,
    result: &mut J,
    gaps: &mut Vec<String>,
    next_steps: &mut Vec<String>,
) {
    result["result_return"] = json!(true);
    result["context_telemetry"] = json!(telemetry);
    push_handoff_gap(s, state, gaps, next_steps);
    result["active_wakeup"] = json!(gaps.is_empty());
    result["safe_rotation"] = json!(safe_rotation(gaps, telemetry, state));
    push_telemetry_gap(gaps, next_steps, telemetry);
    push_rotation_gap(gaps, next_steps, state);
}

/// Verify the requested current session, never a healthy but unrelated old guard.
pub fn readiness(root: &Path, session: Option<&str>, factory: &ReceiverFactory) -> Result<J> {
    load_readiness(root, session, factory)
        .map(|(result, gaps, next_steps)| finish_readiness(result, gaps, next_steps))
}

fn load_readiness(
    root: &Path,
    session: Option<&str>,
    factory: &ReceiverFactory,
) -> Result<(J, Vec<String>, Vec<String>)> {
    crate::queue::load_queue(root).and_then(|s| readiness_checks(root, &s, session, factory))
}

fn readiness_checks(
    root: &Path,
    s: &J,
    session: Option<&str>,
    factory: &ReceiverFactory,
) -> Result<(J, Vec<String>, Vec<String>)> {
    readiness_report(root, s, session).map(|(result, gaps, next_steps)| {
        prepare_readiness(root, s, session, factory, result, gaps, next_steps)
    })
}

fn prepare_readiness(
    root: &Path,
    s: &J,
    session: Option<&str>,
    factory: &ReceiverFactory,
    mut result: J,
    mut gaps: Vec<String>,
    mut next_steps: Vec<String>,
) -> (J, Vec<String>, Vec<String>) {
    push_session_gap(session, &mut gaps, &mut next_steps);
    push_queue_gap(s, &mut gaps, &mut next_steps);
    push_watch_gap(root, &mut gaps, &mut next_steps);
    record_guard(s, session, factory, &mut result, &mut gaps, &mut next_steps);
    (result, gaps, next_steps)
}

fn finish_readiness(mut result: J, gaps: Vec<String>, next_steps: Vec<String>) -> J {
    result["gaps"] = json!(gaps);
    result["next_steps"] = json!(next_steps);
    result
}

fn push_session_gap(session: Option<&str>, gaps: &mut Vec<String>, next_steps: &mut Vec<String>) {
    if session.is_none() {
        gaps.push("current receiver session is required".to_string());
        next_steps.push(
            "run from the exact receiving session (CODEX_THREAD_ID for Codex); do not borrow another pane"
                .to_string(),
        );
    }
}

fn queue_not_ready(s: &J) -> bool {
    model::bool_or(s, "stopped", false) || !model::bool_or(s, "notify", false)
}

fn push_queue_gap(s: &J, gaps: &mut Vec<String>, next_steps: &mut Vec<String>) {
    if queue_not_ready(s) {
        gaps.push("queue is stopped or notifications are disabled".to_string());
        next_steps.push(
            "after verifying ownership, enable the existing queue; do not create a second queue"
                .to_string(),
        );
    }
}

fn push_watch_gap(root: &Path, gaps: &mut Vec<String>, next_steps: &mut Vec<String>) {
    let healthy = crate::queue::artifacts(root)
        .map(|path| model::bool_or(&watch_service::health(&path), "healthy", false))
        .unwrap_or(false);
    if !healthy {
        gaps.push("queue watcher is not healthy; restart the existing watcher".to_string());
        next_steps.push(
            "inspect the queue watcher reason; restart only the verified existing watcher and check its lock/heartbeat"
                .to_string(),
        );
    }
}

fn guard_present(s: &J) -> bool {
    model::get(s, "guard_dir")
        .map(|value| model::truthy(Some(value)))
        .unwrap_or(false)
}

fn push_no_guard_gap(gaps: &mut Vec<String>, next_steps: &mut Vec<String>) {
    gaps.push("no receiver guard; register the current native TUI bridge".to_string());
    next_steps.push(
        "register the current native TUI bridge and exact session; do not reuse an old guard"
            .to_string(),
    );
}

fn record_guard(
    s: &J,
    session: Option<&str>,
    factory: &ReceiverFactory,
    result: &mut J,
    gaps: &mut Vec<String>,
    next_steps: &mut Vec<String>,
) {
    if !guard_present(s) {
        push_no_guard_gap(gaps, next_steps);
        return;
    }
    record_guard_verification(s, session, factory, result, gaps, next_steps);
}

fn record_guard_verification(
    s: &J,
    session: Option<&str>,
    factory: &ReceiverFactory,
    result: &mut J,
    gaps: &mut Vec<String>,
    next_steps: &mut Vec<String>,
) {
    let Some(session) = session else {
        return;
    };
    let evaluation = verify_receiver(s, session, factory, result, gaps, next_steps);
    record_guard_error(evaluation, gaps, next_steps);
}

fn receiver_error_message(error: &FlowError) -> String {
    if matches!(error.kind, ErrorKind::Progress | ErrorKind::Guard) {
        error.message.clone()
    } else {
        "receiver state unavailable".to_string()
    }
}

fn record_guard_error(
    evaluation: Result<()>,
    gaps: &mut Vec<String>,
    next_steps: &mut Vec<String>,
) {
    if let Err(error) = evaluation {
        gaps.push(receiver_error_message(&error));
        next_steps.push(
            "inspect context_guard status and the exact pane/session; do not reuse an old guard or bypass the mutex"
                .to_string(),
        );
    }
}

pub fn receiver_session(s: &J, session: Option<&str>) -> Result<()> {
    let Some(session) = session.filter(|value| !value.is_empty()) else {
        return Err(FlowError::progress(
            "task mutation requires the registered coordinator session",
        ));
    };
    let Some(guard_dir) = model::string(s, "guard_dir") else {
        return Err(FlowError::progress(
            "task mutation requires the registered coordinator session",
        ));
    };
    let path = PathBuf::from(guard_dir);
    let _lock = guard::locked(&path).map_err(|error| {
        FlowError::progress(format!(
            "task mutation receiver registration/session changed: {error}"
        ))
    })?;
    let state = crate::util::read_state_json(&path, "state.json")?;
    if guard::scope(&state) != model::get(s, "receiver").cloned().unwrap_or(Value::Null)
        || model::string(&state, "session").as_deref() != Some(session)
        || model::string(&state, "phase").as_deref() != Some("monitoring")
    {
        return Err(FlowError::progress(
            "task mutation receiver registration/session changed",
        ));
    }
    Ok(())
}
