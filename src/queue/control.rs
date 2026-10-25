//! Queue-owned bootstrap reservation. Directory work belongs to task_env.
use super::*;
use crate::task_env::instance::{allocation, CreateSpec};

/// Caller holds the queue transaction throughout reserved -> ready.
pub(super) fn ensure(root: &Path, s: &mut J, spec: &CreateSpec) -> Result<()> {
    let request = allocation::control_request(root, spec)?;
    let current = s["lifecycle"]["control_bootstrap"].clone();
    if current.is_null() {
        checkpoint(root, s, json!({"reservation":request,"ready":null}))?;
    } else {
        require_reservation(&current, &request)?;
    }
    let bootstrap = s["lifecycle"]["control_bootstrap"].clone();
    if !bootstrap["ready"].is_null() {
        return allocation::verify_control(&bootstrap["reservation"], &bootstrap["ready"]);
    }
    let ready = allocation::initialize_control(&bootstrap["reservation"])?;
    allocation::verify_control(&bootstrap["reservation"], &ready)?;
    checkpoint(
        root,
        s,
        json!({"reservation":bootstrap["reservation"],"ready":ready}),
    )
}

fn require_reservation(current: &J, request: &J) -> Result<()> {
    if current["reservation"] != *request {
        return Err(queue_error("control bootstrap reservation changed"));
    }
    Ok(())
}

/// Only this narrow transaction can change lifecycle.control_bootstrap.
/// Ordinary save continues to require identical lifecycle documents.
fn checkpoint(root: &Path, s: &mut J, bootstrap: J) -> Result<()> {
    isolation::validate_layout(root, s)?;
    let current = super::load_queue(root)?;
    isolation::validate_version(&current, s)?;
    if &current != s {
        return Err(queue_error(
            "control bootstrap requires the current queue snapshot",
        ));
    }
    validate_transition(&current["lifecycle"]["control_bootstrap"], &bootstrap)?;
    let mut next = current;
    next["lifecycle"]["control_bootstrap"] = bootstrap;
    isolation::advance_version(&mut next)?;
    next["updated_at"] = json!(util::now());
    util::atomic_write_json(&root.join("queue.json"), &next)?;
    *s = next;
    Ok(())
}

fn validate_transition(old: &J, next: &J) -> Result<()> {
    if old.is_null() {
        if next["reservation"].is_null() || !next["ready"].is_null() {
            return Err(queue_error(
                "control bootstrap must reserve before initialization",
            ));
        }
        return Ok(());
    }
    require_reservation(old, &next["reservation"])?;
    if !old["ready"].is_null() || next["ready"].is_null() {
        return Err(queue_error(
            "control bootstrap permits only reserved to ready",
        ));
    }
    Ok(())
}
