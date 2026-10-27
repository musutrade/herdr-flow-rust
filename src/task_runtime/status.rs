//! Read-only runtime status. Native journal recovery is a separate operation.
use super::*;

pub fn status(env: &TaskEnvironment, root: &Path) -> Result<J> {
    let runtime = super::verify_bounded(env, root, 1024 * 1024)?;
    Ok(
        json!({"schema":1,"scope":"runtime_journal_snapshot","adapter":runtime["adapter"],
        "supported":false,"markers":"not_sampled","guard":"not_sampled",
        "consistency":"not_sampled","identity":"not_sampled","submission_evidence":"not_sampled",
        "collect_available":false,"safe_to_retry":false,"operator_action":"unsupported_adapter"}),
    )
}
