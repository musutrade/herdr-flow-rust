//! One original Recovery role and observer, never a newly inferred path.
use super::*;

pub(super) fn validate(records: &[J]) -> Result<()> {
    let first = &records[0];
    let origin = path(&first["from"], "runtime")?;
    let model = &first["configuration"]["role_models"]["recovery"];
    let mut current = json!({"generation":0,"runtime":first["from"]["runtime"],
        "scope":first["from"]["scope"],"transaction_id":null});
    for record in records {
        ensure(
            record["configuration"]["role_models"]["recovery"] == *model,
            "handoff cannot replace the original Recovery role",
        )?;
        if configured(record, !model.is_null())? {
            reference(record, &origin, &current)?;
            if record["phase"] == "complete" {
                current = record["recovery"]["to_target"].clone();
            }
        }
    }
    Ok(())
}

fn configured(record: &J, configured: bool) -> Result<bool> {
    ensure(
        record["recovery"].is_null() != configured,
        "handoff Recovery reference differs from its configured role",
    )?;
    Ok(configured)
}

fn reference(record: &J, origin: &Path, current: &J) -> Result<()> {
    let reference = &record["recovery"];
    fields(
        reference,
        &[
            "observer",
            "origin_coordinator",
            "from_target",
            "to_target",
            "phase",
        ],
    )?;
    ensure(
        path(reference, "origin_coordinator")? == origin
            && path(reference, "observer")? == origin.join("recovery-observer"),
        "Recovery observer escaped its original coordinator",
    )?;
    fields(
        &reference["from_target"],
        &["generation", "runtime", "scope", "transaction_id"],
    )?;
    ensure(
        reference["from_target"] == *current,
        "Recovery predecessor generation or transaction changed",
    )?;
    target(record)?;
    phase(reference, record)
}

fn target(record: &J) -> Result<()> {
    let target = &record["recovery"]["to_target"];
    fields(
        target,
        &["generation", "runtime", "scope", "transaction_id"],
    )?;
    ensure(
        target["generation"] == record["generation"]
            && target["runtime"] == record["to"]["runtime"]
            && target["scope"] == record["to"]["scope"],
        "Recovery successor generation/scope changed",
    )?;
    ensure(
        target["transaction_id"] == record["binding"]["transaction_id"],
        "Recovery successor transaction changed",
    )
}

fn phase(reference: &J, record: &J) -> Result<()> {
    ensure(
        matches!(reference["phase"].as_str(), Some("pending" | "bound")),
        "invalid Recovery phase",
    )?;
    ensure(
        reference["phase"] != "bound" || !record["binding"].is_null(),
        "Recovery retarget precedes coordinator binding",
    )?;
    ensure(
        record["phase"] != "complete" || reference["phase"] == "bound",
        "completed handoff has not retargeted Recovery",
    )
}
