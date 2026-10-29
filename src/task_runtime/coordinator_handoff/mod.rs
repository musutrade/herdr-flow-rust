//! Explicit, bounded replacement of a settled task coordinator. The queue is
//! authoritative; journals never grant an independent permission to act.
use super::*;
use crate::lifecycle::Owner;
mod anchor;
mod configuration;
mod gate;
mod mirror;
mod record;
mod recovery;
mod request;
mod transition;

pub(crate) const KEY: &str = "receiver_handoff";
const GENERATIONS: usize = 8;
const RECORD_BYTES: usize = 32 * 1024;
const STATE_BYTES: usize = 256 * 1024;

pub(crate) use gate::{admit_control, check_workspace, runtime_blocked};
pub(crate) use record::validate_queue;
pub(crate) use request::{action, status};
pub(crate) use transition::validate_save;

fn fields(value: &J, names: &[&str]) -> Result<()> {
    let object = value
        .as_object()
        .ok_or_else(|| FlowError::guard("coordinator handoff record must be an object"))?;
    ensure(
        object.len() == names.len() && names.iter().all(|name| object.contains_key(*name)),
        "coordinator handoff fields changed",
    )
}

fn bounded(value: &J, limit: usize) -> Result<()> {
    ensure(
        serde_json::to_vec(value)?.len() <= limit,
        "coordinator handoff exceeds its storage bound",
    )
}

fn text(value: &J, key: &str, limit: usize) -> Result<String> {
    let text = required(value, key)?;
    ensure(
        text.len() <= limit,
        "coordinator handoff text exceeds its bound",
    )?;
    Ok(text)
}

fn digest(value: &J, key: &str) -> Result<()> {
    let value = text(value, key, 64)?;
    ensure(
        value.len() == 64
            && value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "coordinator handoff digest is invalid",
    )
}

fn path(value: &J, key: &str) -> Result<PathBuf> {
    let path = PathBuf::from(text(value, key, 4096)?);
    let normalized: PathBuf = path.components().collect();
    ensure(
        path.is_absolute()
            && path.as_os_str() == normalized.as_os_str()
            && path.components().all(|part| {
                matches!(
                    part,
                    std::path::Component::RootDir | std::path::Component::Normal(_)
                )
            }),
        "coordinator handoff path must be absolute and normalized",
    )?;
    Ok(path)
}

fn phase(record: &J) -> Result<usize> {
    [
        "requested",
        "draining",
        "creating",
        "prepared",
        "bound",
        "accepted",
        "retiring",
        "complete",
        "cancelled",
    ]
    .iter()
    .position(|name| record["phase"] == *name)
    .ok_or_else(|| FlowError::guard("unknown coordinator handoff phase"))
}

fn settled(record: &J) -> bool {
    matches!(record["phase"].as_str(), Some("complete" | "cancelled"))
}

#[cfg(test)]
#[path = "../../../tests/common/coordinator_handoff_records.rs"]
mod tests;
