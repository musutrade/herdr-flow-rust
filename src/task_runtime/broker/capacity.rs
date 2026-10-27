//! Resource pressure is evidence for inspection, never a business task outcome.
use super::*;

pub(in crate::task_runtime) fn tools(root: &Path, name: &str, journal: &J) -> Result<()> {
    let count = journal
        .as_object()
        .ok_or_else(|| FlowError::guard("invalid tool journal"))?
        .len();
    // Match atomic_write_json's actual pretty representation, including newline.
    let bytes = serde_json::to_vec_pretty(journal)?.len() + 1;
    check(root, name, count, bytes, 1024 * 1024)
}

pub(super) fn events(root: &Path, journal: &J) -> Result<()> {
    let count = ["turns", "pending"]
        .iter()
        .filter_map(|key| journal[key].as_object().map(|m| m.len()))
        .max()
        .unwrap_or(0);
    check(
        root,
        "broker-events.json",
        count,
        serde_json::to_vec_pretty(journal)?.len() + 1,
        16 * 1024 * 1024,
    )
}

fn check(root: &Path, name: &str, count: usize, bytes: usize, byte_limit: usize) -> Result<()> {
    if count <= 64 && bytes <= byte_limit {
        return Ok(());
    }
    util::atomic_write_json(
        &root.join("record-capacity.json"),
        &json!({
            "phase":"blocked","journal":name,"entries":count,"bytes":bytes,
            "entry_limit":64,"byte_limit":byte_limit,"replay_allowed":false,
            "next_step":"Inspect the original task and unresolved operations. Finish or reconcile the current turn; completed previous turns can be checkpointed. Do not replay unknown operations or create a replacement task."
        }),
    )?;
    Err(FlowError::new(
        crate::ErrorKind::OutputLimit,
        "record capacity blocked; inspect record-capacity.json; original task retained",
    ))
}
