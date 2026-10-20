//! Read-only diagnosis of watcher and session phases.

use super::external_delivery_pending;
use crate::model;
use crate::model::J;
use serde_json::json;
use serde_json::Value;

pub fn diagnosis(s: &J, watcher: &J) -> J {
    if !model::bool_or(watcher, "healthy", false) {
        return diagnosis_watcher_unhealthy(watcher);
    }
    let phase = model::string(s, "phase").unwrap_or_else(|| "unknown".to_string());
    diagnosis_phase(s, &phase)
}

fn diagnosis_watcher_unhealthy(watcher: &J) -> J {
    json!({
        "classification": "watcher_unhealthy",
        "reason": model::string(watcher, "reason").unwrap_or_else(|| "unknown".to_string()),
        "next_step": model::string(watcher, "next_step").unwrap_or_else(|| "inspect watch.log and the process identity; restart only the verified watcher; do not reuse an old guard".to_string())
    })
}

fn diagnosis_phase(s: &J, phase: &str) -> J {
    if phase == "needs_attention" {
        return diagnosis_needs_attention(s);
    }
    diagnosis_delivery(s, phase)
}

fn diagnosis_needs_attention(s: &J) -> J {
    json!({
        "classification": "needs_attention",
        "reason": model::string(s, "error").unwrap_or_else(|| "guard needs attention".to_string()),
        "next_step": "inspect the recorded error and current pane/session; reconcile only the existing state; never replay an ambiguous action"
    })
}

fn diagnosis_delivery(s: &J, phase: &str) -> J {
    if external_delivery_pending(s) {
        return json!({
            "classification": "external_delivery",
            "reason": "a queue delivery is pending",
            "next_step": "ACK or explicitly resolve the existing delivery before checkpointing; do not resend"
        });
    }
    diagnosis_rotation(s, phase)
}

fn diagnosis_rotation(s: &J, phase: &str) -> J {
    if model::bool_or(s, "rotation_due", false) {
        return json!({
            "classification": "rotation_due",
            "reason": "context threshold reached",
            "next_step": "finish a safe checkpoint and rotate; do not dispatch into this role during handoff"
        });
    }
    diagnosis_mutating(s, phase)
}

fn diagnosis_mutating(s: &J, phase: &str) -> J {
    if matches!(
        phase,
        "request_sending"
            | "review_config_sending"
            | "runtime_restore_sending"
            | "tui_launch_sending"
    ) {
        return json!({
            "classification": phase,
            "reason": "a mutating action is in flight",
            "next_step": "inspect the exact submission and current runtime; use the matching reconcile path only after inspection; never replay"
        });
    }
    diagnosis_successor(s, phase)
}

fn diagnosis_successor(s: &J, phase: &str) -> J {
    if matches!(
        phase,
        "awaiting_shell" | "awaiting_tui" | "successor_ready" | "awaiting_ack"
    ) {
        return json!({
            "classification": phase,
            "reason": "successor startup or acknowledgement is in progress",
            "next_step": "inspect the successor pane, bridge marker and exact session; do not launch over another process or reuse the predecessor"
        });
    }
    diagnosis_requested(s, phase)
}

fn diagnosis_requested(s: &J, phase: &str) -> J {
    if phase == "requested" {
        return json!({
            "classification": phase,
            "reason": "a checkpoint/rotation request is recorded",
            "next_step": "prepare the handoff at the next safe boundary; do not interrupt an active turn"
        });
    }
    diagnosis_parked(s, phase)
}

fn diagnosis_parked(s: &J, phase: &str) -> J {
    if phase == "parked" {
        return json!({
            "classification": "parked",
            "reason": "worker is parked after a completed assignment",
            "next_step": "wait for a new pending assignment ID; do not reuse the completed assignment"
        });
    }
    diagnosis_released(s, phase)
}

fn diagnosis_released(s: &J, phase: &str) -> J {
    if phase == "released" {
        return json!({
            "classification": "released",
            "reason": "guard is released",
            "next_step": "register a new guard only for a new task/assignment; do not resume the released session implicitly"
        });
    }
    diagnosis_telemetry(s, phase)
}

fn diagnosis_telemetry(s: &J, phase: &str) -> J {
    if model::get(s, "usage").map(Value::is_null).unwrap_or(true) {
        return json!({
            "classification": "telemetry_unknown",
            "reason": "context usage is unknown",
            "next_step": "inspect the transcript identity and guard observation; do not claim automatic protection or raise thresholds"
        });
    }
    diagnosis_healthy(phase)
}

fn diagnosis_healthy(phase: &str) -> J {
    json!({
        "classification": if phase == "monitoring" { "monitoring" } else { phase },
        "reason": "guard is healthy",
        "next_step": "no action; continue monitoring or dispatch within the current authorization"
    })
}
