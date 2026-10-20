//! Role-level continuation policy; no CI, hosting or test-product requirements.
use super::*;
pub(super) fn validate_next(verdict: &str, action: &J) -> Result<()> {
    let expected = match verdict {
        "passed" => "delivery",
        "failed" => "repair",
        "unable" | "execution_unavailable" => "restore",
        "ambiguous" => "clarify",
        _ => return Err(progress_error("unknown acceptance conclusion")),
    };
    ensure(
        action["kind"] == expected,
        "continuation contradicts independent verdict",
    )?;
    for key in ["owner", "reason", "condition"] {
        text(model::get(action, key), key, 1000)?;
    }
    Ok(())
}
pub(super) fn validate_resume(parent: &J, spec: &J) -> Result<()> {
    let intent = &parent["continuation"];
    ensure(intent["phase"] == "planned", "continuation is not pending")?;
    ensure(
        parent["state_version"] == spec["state_version"],
        "parent version changed",
    )?;
    validate_binding(parent, spec)?;
    let revision = parent["requirement_revision"].to_string();
    ensure_dispatch_authorized(&parent["requirements"][revision], spec)?;
    let role = successor_role(intent)?;
    ensure(spec["activity"]["role"] == role, "wrong continuation role")?;
    text(
        model::get(spec, "resolution"),
        "restored condition or clarification evidence",
        1000,
    )?;
    validate_clarification(parent, intent)?;
    Ok(())
}

fn validate_binding(parent: &J, spec: &J) -> Result<()> {
    let intent = &parent["continuation"];
    if intent["action"]["kind"] != "clarify" {
        ensure(
            parent["candidate"] == intent["binding"]["candidate"]
                && parent["acceptance_round"] == intent["binding"]["lineage"]["round"],
            "continuation candidate or round changed",
        )?;
        ensure(
            parent["requirement_revision"] == intent["binding"]["lineage"]["revision"],
            "continuation requirements changed",
        )?;
        ensure(
            spec["activity"]["input"] == intent["binding"]["candidate"],
            "continuation input changed",
        )?;
    }
    Ok(())
}
fn successor_role(intent: &J) -> Result<&'static str> {
    let role = match intent["action"]["kind"].as_str() {
        Some("repair") => "development",
        Some("restore" | "clarify") => "acceptance",
        _ => {
            return Err(progress_error(
                "delivery uses its own authorized delivery protocol",
            ))
        }
    };
    Ok(role)
}
fn validate_clarification(parent: &J, intent: &J) -> Result<()> {
    if intent["action"]["kind"] == "clarify" {
        ensure(
            parent["requirement_revision"].as_i64()
                > intent["binding"]["lineage"]["revision"].as_i64(),
            "confirmed clarification must revise requirements before resuming",
        )?;
    }
    Ok(())
}
