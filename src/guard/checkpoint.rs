//! Checkpoint persistence and fresh successor validation.

use super::{
    external_delivery_pending, fingerprint, guard_error, read_state, save, transcript_state,
    verify_handoff,
};
use crate::error::Result;
use crate::model;
use crate::model::J;
use crate::util;
use crate::util::pyjson::digest;
use serde_json::json;
use serde_json::Value;
use std::path::Path;
use std::path::PathBuf;

pub fn checkpoint(root: &Path, path: &Path, session: &str) -> Result<J> {
    let s = read_state(root)?;
    checkpoint_state(root, path, session, s)
}

fn checkpoint_state(root: &Path, path: &Path, session: &str, s: J) -> Result<J> {
    checkpoint_version(&s)?;
    checkpoint_ready(root, path, session, s)
}

fn checkpoint_version(s: &J) -> Result<()> {
    if model::int(s, "version") == Some(2) {
        return Ok(());
    }
    Err(guard_error("legacy guard requires explicit migrate"))
}

fn checkpoint_ready(root: &Path, path: &Path, session: &str, s: J) -> Result<J> {
    checkpoint_session_phase(&s, session)?;
    checkpoint_handoff(root, path, session, s)
}

fn checkpoint_session_phase(s: &J, session: &str) -> Result<()> {
    [
        model::string(s, "session").as_deref() == Some(session),
        matches!(
            model::string(s, "phase").as_deref(),
            Some("monitoring" | "requested" | "parked")
        ),
    ]
    .into_iter()
    .all(|valid| valid)
    .then_some(())
    .ok_or_else(|| guard_error("checkpoint session or phase mismatch"))
}

fn checkpoint_handoff(root: &Path, path: &Path, session: &str, s: J) -> Result<J> {
    if external_delivery_pending(&s) {
        return Err(guard_error(
            "acknowledge pending wait-queue delivery before checkpoint",
        ));
    }
    checkpoint_verify(root, path, session, s)
}

fn checkpoint_verify(root: &Path, path: &Path, session: &str, s: J) -> Result<J> {
    let raw = verify_handoff(&s, path)?;
    checkpoint_parse(root, session, s, raw)
}

fn checkpoint_parse(root: &Path, session: &str, s: J, raw: Vec<u8>) -> Result<J> {
    let handoff: J = serde_json::from_slice(&raw)?;
    checkpoint_assignment(root, session, s, handoff)
}

fn checkpoint_assignment(root: &Path, session: &str, s: J, handoff: J) -> Result<J> {
    let assignment = model::get(&handoff, "assignment").cloned();
    checkpoint_assignment_check(&s, assignment.as_ref())?;
    checkpoint_transcript(root, session, s, handoff, assignment)
}

fn checkpoint_assignment_check(s: &J, assignment: Option<&J>) -> Result<()> {
    let completed = checkpoint_completed_ids(s);
    checkpoint_assignment_status(s, assignment, &completed)
}

fn checkpoint_completed_ids(s: &J) -> Vec<String> {
    model::array_field(s, "completed_assignments")
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(ToString::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn checkpoint_assignment_status(s: &J, assignment: Option<&J>, completed: &[String]) -> Result<()> {
    let Some(assignment) = checkpoint_assignment_value(assignment) else {
        return checkpoint_parked_requires_assignment(s);
    };
    checkpoint_pending_assignment(assignment, completed)
}

fn checkpoint_assignment_value(assignment: Option<&J>) -> Option<&J> {
    assignment.filter(|value| !value.is_null())
}

fn checkpoint_pending_assignment(assignment: &J, completed: &[String]) -> Result<()> {
    let id = model::string(assignment, "id").unwrap_or_default();
    let blocked = [
        model::string(assignment, "status").as_deref() == Some("pending"),
        completed.contains(&id),
    ]
    .into_iter()
    .all(|hit| hit);
    if blocked {
        return Err(guard_error(
            "completed assignment cannot be resumed; use a new assignment ID",
        ));
    }
    Ok(())
}

fn checkpoint_parked_requires_assignment(s: &J) -> Result<()> {
    if model::string(s, "phase").as_deref() == Some("parked") {
        return Err(guard_error(
            "parked worker requires an explicit new assignment",
        ));
    }
    Ok(())
}

fn checkpoint_transcript(
    root: &Path,
    session: &str,
    s: J,
    handoff: J,
    assignment: Option<J>,
) -> Result<J> {
    let transcript_path =
        model::string(&s, "transcript").ok_or_else(|| guard_error("transcript is unavailable"))?;
    checkpoint_transcript_data(root, session, s, handoff, assignment, &transcript_path)
}

fn checkpoint_transcript_data(
    root: &Path,
    session: &str,
    s: J,
    handoff: J,
    assignment: Option<J>,
    transcript_path: &str,
) -> Result<J> {
    let data = transcript_state(Path::new(transcript_path), None)?;
    checkpoint_transcript_identity(root, session, s, handoff, assignment, data)
}

fn checkpoint_transcript_identity(
    root: &Path,
    session: &str,
    s: J,
    handoff: J,
    assignment: Option<J>,
    data: J,
) -> Result<J> {
    if model::string(&data, "session").as_deref() != Some(session) {
        return Err(guard_error("transcript changed"));
    }
    checkpoint_write(root, s, handoff, assignment, data)
}

fn checkpoint_write(root: &Path, s: J, handoff: J, assignment: Option<J>, data: J) -> Result<J> {
    let handoff_path = util::resolve_path(root).join("handoff.json");
    checkpoint_store_handoff(&handoff_path, &handoff)?;
    checkpoint_after_store(root, s, &handoff_path, assignment, data)
}

fn checkpoint_store_handoff(handoff_path: &Path, handoff: &J) -> Result<()> {
    util::atomic_write_json(handoff_path, handoff)
}

fn checkpoint_after_store(
    root: &Path,
    mut s: J,
    handoff_path: &Path,
    assignment: Option<J>,
    data: J,
) -> Result<J> {
    checkpoint_advisor_source(&mut s, handoff_path);
    checkpoint_set_digest(&mut s, handoff_path)?;
    checkpoint_set_checkout(root, s, assignment, data)
}

fn checkpoint_advisor_source(s: &mut J, handoff_path: &Path) {
    if model::get(s, "advisor")
        .map(|value| model::truthy(Some(value)))
        .unwrap_or(false)
    {
        s["advisor"]["source"] = json!(handoff_path.display().to_string());
    }
}

fn checkpoint_set_digest(s: &mut J, handoff_path: &Path) -> Result<()> {
    s["handoff_sha"] = json!(digest(&util::read_bytes(handoff_path)?));
    Ok(())
}

fn checkpoint_set_checkout(root: &Path, mut s: J, assignment: Option<J>, data: J) -> Result<J> {
    let checkout = checkpoint_fingerprint(&s)?;
    checkpoint_fields(&mut s, checkout, &data);
    checkpoint_save_completed(root, &mut s, assignment)
}

fn checkpoint_fingerprint(s: &J) -> Result<String> {
    fingerprint(&model::string(s, "cwd").unwrap_or_default())
}

fn checkpoint_fields(s: &mut J, checkout: String, data: &J) {
    s["phase"] = json!("ready");
    s["checkout"] = json!(checkout);
    s["checkpoint_users"] = data
        .get("user_messages")
        .cloned()
        .unwrap_or_else(|| json!(0));
    s["checkpoint_at"] = json!(util::now());
}

fn checkpoint_save_completed(root: &Path, s: &mut J, assignment: Option<J>) -> Result<J> {
    checkpoint_completed_update(s, assignment);
    checkpoint_save(root, s)
}

fn checkpoint_completed_update(s: &mut J, assignment: Option<J>) {
    let Some(assignment) = checkpoint_assignment_value(assignment.as_ref()) else {
        return;
    };
    if model::string(assignment, "status").as_deref() == Some("completed") {
        checkpoint_park(s, assignment);
    }
}

fn checkpoint_park(s: &mut J, assignment: &J) {
    let id = model::string(assignment, "id").unwrap_or_default();
    let mut ids = checkpoint_completed_ids(s);
    checkpoint_push_id(&mut ids, id);
    s["completed_assignments"] = json!(ids);
    s["phase"] = json!("parked");
    s["rotation_due"] = json!(false);
}

fn checkpoint_push_id(ids: &mut Vec<String>, id: String) {
    if !ids.contains(&id) {
        ids.push(id);
    }
}

fn checkpoint_save(root: &Path, s: &mut J) -> Result<J> {
    save(root, s)?;
    Ok(s.clone())
}

pub fn validate_ready(root: &Path, s: &mut J) -> Result<()> {
    let path = util::resolve_path(root).join("handoff.json");
    if digest(&util::read_bytes(&path)?) != model::string(s, "handoff_sha").unwrap_or_default() {
        return Err(guard_error("handoff modified after checkpoint"));
    }
    verify_handoff(s, &path)?;
    if fingerprint(&model::string(s, "cwd").unwrap_or_default())?
        != model::string(s, "checkout").unwrap_or_default()
    {
        return Err(guard_error("checkout changed after checkpoint"));
    }
    let transcript_path =
        model::string(s, "transcript").ok_or_else(|| guard_error("transcript is unavailable"))?;
    let cursor = model::get(s, "ready_scan").cloned();
    let data = transcript_state(Path::new(&transcript_path), cursor.as_ref())?;
    s["ready_scan"] = data.get("cursor").cloned().unwrap_or(Value::Null);
    if model::int(&data, "user_messages").unwrap_or(0)
        != model::int(s, "checkpoint_users").unwrap_or(0)
    {
        return Err(guard_error("new user input after checkpoint; do not reset"));
    }
    Ok(())
}

pub fn validate_fresh_successor(s: &J) -> Result<()> {
    let successor = model::string(s, "successor")
        .ok_or_else(|| guard_error("successor identity is unavailable"))?;
    validate_fresh_sessions(s, &successor)
}

fn validate_fresh_sessions(s: &J, successor: &str) -> Result<()> {
    let sessions_dir = model::string(s, "sessions_dir")
        .ok_or_else(|| guard_error("sessions directory is unavailable"))?;
    let matches = util::files_with_suffix(Path::new(&sessions_dir), &format!("{successor}.jsonl"));
    validate_fresh_matches(s, successor, &matches)
}

fn validate_fresh_matches(s: &J, successor: &str, matches: &[PathBuf]) -> Result<()> {
    if matches.len() > 1 {
        return Err(guard_error("successor transcript identity is ambiguous"));
    }
    validate_fresh_path(s, successor, matches.first())
}

fn validate_fresh_path(s: &J, successor: &str, path: Option<&PathBuf>) -> Result<()> {
    let Some(path) = path else {
        return Ok(());
    };
    validate_fresh_read(s, successor, path)
}

fn validate_fresh_read(s: &J, successor: &str, path: &Path) -> Result<()> {
    let data = transcript_state(path, None)?;
    validate_fresh_data(s, successor, &data)
}

fn validate_fresh_data(s: &J, successor: &str, data: &J) -> Result<()> {
    if validate_fresh_mismatch(s, successor, data) {
        return Err(guard_error(
            "successor already received input before its handoff",
        ));
    }
    Ok(())
}

fn validate_fresh_mismatch(s: &J, successor: &str, data: &J) -> bool {
    [
        model::string(data, "session").as_deref() == Some(successor),
        model::string(data, "cwd") == model::string(s, "cwd"),
        model::int(data, "user_messages").unwrap_or(0) == 0,
    ]
    .into_iter()
    .any(|ok| !ok)
}
