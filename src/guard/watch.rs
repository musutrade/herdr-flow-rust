//! Watcher locking, ticks and durable failure handling.

use super::{guard_error, locked, once, read_state, save, OBSERVATION_PHASES, TRANSIENT_LIMIT};
use crate::error::ErrorKind;
use crate::error::FlowError;
use crate::error::Result;
use crate::herdr::GuardAdapter;
use crate::herdr::Herdr;
use crate::logging;
use crate::model;
use crate::model::J;
use crate::util;
use crate::watch_service;
use serde_json::json;
use std::path::Path;

pub fn validate_watch(root: &Path) -> Result<()> {
    let s = read_state(root)?;
    validate_watch_version(&s)
}

fn validate_watch_version(s: &J) -> Result<()> {
    if model::int(s, "version") != Some(2) {
        return Err(guard_error("legacy guard requires explicit migrate"));
    }
    validate_watch_phase(s)
}

fn validate_watch_phase(s: &J) -> Result<()> {
    if model::string(s, "phase").as_deref() == Some("released") {
        return Err(guard_error("released guard cannot be watched"));
    }
    Ok(())
}

fn read_state_or_empty(root: &Path) -> J {
    read_state(root).unwrap_or_else(|_| json!({}))
}

fn guard_failure(root: &Path, h: &mut dyn GuardAdapter, notify: bool, error: &FlowError) -> J {
    let s = read_state_or_empty(root);
    if guard_failure_attention(&s) {
        return s;
    }
    let transient = guard_failure_transient(error);
    let message = guard_failure_message(error);
    guard_failure_log(&s, error, transient);
    let phase = model::string(&s, "phase").unwrap_or_default();
    guard_failure_route(root, h, notify, s, phase, message, transient)
}

fn guard_failure_attention(s: &J) -> bool {
    model::string(s, "phase").as_deref() == Some("needs_attention")
}

fn guard_failure_transient(error: &FlowError) -> bool {
    matches!(error.kind, ErrorKind::Transient | ErrorKind::Timeout)
}

fn guard_failure_message(error: &FlowError) -> String {
    if error.message.is_empty() {
        return error.kind.type_name().to_string();
    }
    error.message.clone()
}

fn guard_failure_log(s: &J, error: &FlowError, transient: bool) {
    logging::log_event_fields(
        "guard_poll_failed",
        "context_guard",
        "guard poll failed",
        logging::Fields {
            task_id: model::string(s, "task").as_deref(),
            guard_epoch: model::u64_field(s, "epoch"),
            metadata: Some(&json!({
                "exception": error.kind.type_name(),
                "phase": s.get("phase"),
                "transient": transient
            })),
            ..Default::default()
        },
    );
}

fn guard_failure_route(
    root: &Path,
    h: &mut dyn GuardAdapter,
    notify: bool,
    s: J,
    phase: String,
    message: String,
    transient: bool,
) -> J {
    if guard_failure_observed(transient, &phase) {
        return guard_failure_transient_state(root, h, notify, s, phase, message);
    }
    guard_failure_final(root, h, notify, s, phase, &message)
}

fn guard_failure_observed(transient: bool, phase: &str) -> bool {
    [transient, OBSERVATION_PHASES.contains(&phase)]
        .into_iter()
        .all(|observed| observed)
}

fn guard_failure_transient_state(
    root: &Path,
    h: &mut dyn GuardAdapter,
    notify: bool,
    mut s: J,
    phase: String,
    message: String,
) -> J {
    let count = model::u64_field(&s, "transient_errors").unwrap_or(0) + 1;
    s["transient_errors"] = json!(count);
    s["last_transient_error"] = json!(message);
    if count < TRANSIENT_LIMIT {
        let _ = save(root, &mut s);
        return s;
    }
    guard_failure_final(root, h, notify, s, phase, &message)
}

fn guard_failure_final(
    root: &Path,
    h: &mut dyn GuardAdapter,
    notify: bool,
    mut s: J,
    phase: String,
    message: &str,
) -> J {
    s["error"] = json!(message);
    s["phase_before_error"] = json!(phase);
    s["phase"] = json!("needs_attention");
    model::remove(&mut s, "transient_errors");
    let _ = save(root, &mut s);
    guard_failure_notify(root, h, notify, s, message)
}

fn guard_failure_notify(
    root: &Path,
    h: &mut dyn GuardAdapter,
    notify: bool,
    s: J,
    message: &str,
) -> J {
    if notify {
        h.set_state(&s);
        let _ = h.call(&[
            "notification".to_string(),
            "show".to_string(),
            "Herdr flow needs attention".to_string(),
            "--body".to_string(),
            format!("{}: {}", root.display(), message),
            "--sound".to_string(),
            "request".to_string(),
        ]);
    }
    s
}

pub fn guarded_once(root: &Path, h: &mut dyn GuardAdapter, notify: bool) -> J {
    once(root, h).map_or_else(
        |error| guarded_once_error(root, h, notify, error),
        |s| guarded_once_ok(root, s),
    )
}

fn guarded_once_ok(root: &Path, mut s: J) -> J {
    if model::get(&s, "transient_errors").is_some() {
        model::remove(&mut s, "transient_errors");
        model::remove(&mut s, "last_transient_error");
        let _ = save(root, &mut s);
    }
    s
}

fn guarded_once_error(root: &Path, h: &mut dyn GuardAdapter, notify: bool, error: FlowError) -> J {
    if error.kind == ErrorKind::AgentBusy {
        return read_state_or_empty(root);
    }
    guard_failure(root, h, notify, &error)
}

fn guard_failure_without_adapter(root: &Path, error: &FlowError) -> J {
    let mut s = read_state_or_empty(root);
    if guard_failure_attention(&s) {
        return s;
    }
    let transient = guard_failure_transient(error);
    let message = guard_failure_message(error);
    guard_failure_log(&s, error, transient);
    let phase = model::string(&s, "phase").unwrap_or_default();
    if guard_failure_observed(transient, &phase) {
        let count = model::u64_field(&s, "transient_errors").unwrap_or(0) + 1;
        s["transient_errors"] = json!(count);
        s["last_transient_error"] = json!(message);
        if count < TRANSIENT_LIMIT {
            let _ = save(root, &mut s);
            return s;
        }
    }
    s["error"] = json!(message);
    s["phase_before_error"] = json!(phase);
    s["phase"] = json!("needs_attention");
    model::remove(&mut s, "transient_errors");
    let _ = save(root, &mut s);
    s
}

pub fn watch(root: &Path, ready_fd: Option<std::os::fd::RawFd>, one: bool) -> Result<()> {
    let root = util::state_dir(root, false)?;
    watch_service::run(
        &root,
        || watch_tick(&root, one),
        || validate_watch(&root),
        10,
        ready_fd,
        one,
    )
}

fn watch_lock(root: &Path, one: bool) -> Result<Option<util::FileLock>> {
    match locked(root) {
        Ok(lock) => Ok(Some(lock)),
        Err(error) if error.kind == ErrorKind::StateBusy => {
            if one {
                return Err(error);
            }
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

fn watch_tick(root: &Path, one: bool) -> Result<bool> {
    let Some(_lock) = watch_lock(root, one)? else {
        return Ok(false);
    };
    let state = read_state(root)?;
    let phase = model::string(&state, "phase").unwrap_or_default();
    if phase == "released" {
        return Ok(true);
    }
    if matches!(phase.as_str(), "parked" | "needs_attention") {
        return watch_report(&state, one);
    }
    if phase == "review_config_sending" {
        let error = guard_error(
            "review configuration is ambiguous; inspect then review-reconcile without replay",
        );
        return watch_report(&guard_failure_without_adapter(root, &error), one);
    }
    watch_active(root, &state, one)
}

fn watch_active(root: &Path, state: &J, one: bool) -> Result<bool> {
    let mut h = match Herdr::new(state) {
        Ok(adapter) => adapter,
        Err(error) => return watch_report(&guard_failure_without_adapter(root, &error), one),
    };
    let state = guarded_once(root, &mut h, true);
    watch_report(&state, one)
}

fn watch_report(state: &J, one: bool) -> Result<bool> {
    if one {
        print_watch_state(state)?;
    }
    Ok(false)
}

fn print_watch_state(s: &J) -> Result<()> {
    println!(
        "{}",
        serde_json::to_string(s)
            .map_err(|error| FlowError::new(ErrorKind::Json, error.to_string()))?
    );
    Ok(())
}
