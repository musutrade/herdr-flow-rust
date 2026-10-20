//! Handoff content, assignment and process-release validation.

use super::guard_error;
use crate::error::Result;
use crate::model;
use crate::model::J;
use crate::util;
use crate::util::pyjson::digest;
use crate::util::pyjson::python_sorted_compact_utf8;
use serde_json::Value;
use std::path::Path;

fn handoff_identity(s: &J, handoff: &J) -> Result<()> {
    for key in ["task", "epoch", "cwd"] {
        if model::get(handoff, key) != model::get(s, key) {
            return Err(guard_error(format!("handoff {key} mismatch")));
        }
    }
    for key in [
        "goal",
        "next_action",
        "authorization",
        "done_when",
        "completed",
        "blockers",
        "preserve",
    ] {
        if model::get(handoff, key).is_none() {
            return Err(guard_error(format!("handoff missing {key}")));
        }
    }
    Ok(())
}

fn handoff_assignment(handoff: &J) -> Result<()> {
    let Some(assignment) = model::get(handoff, "assignment").filter(|value| !value.is_null())
    else {
        return Ok(());
    };
    let valid = assignment.is_object()
        && model::string(assignment, "id")
            .map(|id| !id.trim().is_empty())
            .unwrap_or(false)
        && matches!(
            model::string(assignment, "status").as_deref(),
            Some("pending" | "completed")
        );
    if !valid {
        return Err(guard_error(
            "assignment requires id and pending/completed status",
        ));
    }
    if model::string(assignment, "status").as_deref() == Some("completed") {
        let evidence = model::string(assignment, "evidence").unwrap_or_default();
        if evidence.is_empty()
            || !Path::new(&evidence).is_absolute()
            || !Path::new(&evidence).is_file()
        {
            return Err(guard_error(
                "completed assignment requires existing absolute evidence file",
            ));
        }
    }
    Ok(())
}

fn handoff_processes(handoff: &J) -> Result<()> {
    if !handoff_processes_released(handoff) {
        return Err(guard_error("writer/commands not released"));
    }
    handoff_processes_validate(handoff)
}

fn handoff_processes_released(handoff: &J) -> bool {
    let writer_released = model::get(handoff, "writer")
        .map(Value::is_null)
        .unwrap_or(true);
    let quiescent = model::bool_or(handoff, "quiescent", false);
    let running_commands = model::get(handoff, "running_commands")
        .and_then(Value::as_array)
        .map(Vec::is_empty)
        .unwrap_or(false);
    [writer_released, quiescent, running_commands]
        .into_iter()
        .all(|released| released)
}

fn handoff_processes_validate(handoff: &J) -> Result<()> {
    model::array_field(handoff, "processes")
        .map(|processes| processes.iter().all(handoff_process_ok))
        .unwrap_or(true)
        .then_some(())
        .ok_or_else(|| guard_error("background process has no independent owner/identity"))
}

fn handoff_process_ok(process: &J) -> bool {
    [
        model::bool_or(process, "independent", false),
        model::truthy(model::get(process, "owner")),
        model::truthy(model::get(process, "identity")),
    ]
    .into_iter()
    .all(|independent| independent)
}

pub fn verify_handoff(s: &J, path: &Path) -> Result<Vec<u8>> {
    let raw = util::read_bytes(path)?;
    if raw.len() > 16000 {
        return Err(guard_error(
            "handoff exceeds 16 KB; move history to references",
        ));
    }
    let handoff: J = serde_json::from_slice(&raw)?;
    if !handoff.is_object() {
        return Err(guard_error("handoff must be a JSON object"));
    }
    handoff_identity(s, &handoff)?;
    handoff_assignment(&handoff)?;
    handoff_processes(&handoff)?;
    Ok(raw)
}

pub fn handoff_digest(handoff: &J) -> String {
    digest(python_sorted_compact_utf8(handoff).as_bytes())
}
