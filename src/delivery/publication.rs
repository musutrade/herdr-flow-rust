//! Bridge for existing PR/batch executors. Publication remains in their saved
//! intents and CI/Gate policy; this read-only bridge rechecks the actual landing.
use super::*;
use crate::ci::GitHubApi;
mod remote;
pub use remote::{confirm_remote, prepare_remote, reserve_remote, revalidate_slot};
pub(super) use remote::{validate_parent, validate_target};

/// `publication` is the PR/lane task already reconciled by the existing path;
/// `member` is the accepted source task (the same task for a direct PR).
/// Customer executors call this from observe, never submit a merge again here.
pub fn verify(
    state: &J,
    member: &J,
    publication: &J,
    artifact: &J,
    api: Option<&dyn GitHubApi>,
) -> Result<Observation> {
    crate::progress::delivery_prerequisites(member)?;
    crate::progress::delivery_prerequisites(publication)?;
    ensure(
        artifact["adapter"] == "git",
        "PR delivery requires a Git artifact",
    )?;
    let record = &publication["pr"];
    ensure(
        record["phase"] == "merged",
        "existing PR path has not confirmed landing",
    )?;
    verify_mapping(member, record, artifact)?;
    let current = reconcile_publication(state, publication, api)?;
    confirmed_landing(&current["pr"], record)
}
fn confirmed_landing(actual: &J, record: &J) -> Result<Observation> {
    ensure(
        actual["phase"] == "merged",
        "PR no longer has a verified landing",
    )?;
    for key in ["head", "commit", "repo", "base_ref", "number"] {
        ensure(
            actual["landing"][key] == record["landing"][key],
            "PR landing identity changed",
        )?;
    }
    Ok(Observation::Delivered(actual["landing"].clone()))
}
fn reconcile_publication(state: &J, publication: &J, api: Option<&dyn GitHubApi>) -> Result<J> {
    let name = field(publication, "task")?;
    let mut task = publication.clone();
    // Only an in-memory read-only reconciliation. The owning queue stays frozen.
    task["control"] = json!("running");
    task["phase"] = json!("blocked");
    let spec = json!({"task":name,"state_version":task["state_version"]});
    // Preserve dependency tasks and lane mappings during the read-only query.
    let mut snapshot = state.clone();
    snapshot["stopped"] = json!(false);
    snapshot["tasks"][name] = task;
    crate::pr::reconcile(&mut snapshot, &spec, util::now(), api)
}
fn verify_mapping(member: &J, record: &J, artifact: &J) -> Result<()> {
    if record["head"] == artifact["id"] {
        return Ok(());
    }
    let mapping = &member["batch_delivery"]["landing"];
    ensure(
        member["batch_delivery"]["phase"] == "landed"
            && mapping["head"] == artifact["id"]
            && mapping["batch_head"] == record["head"]
            && mapping["commit"] == record["landing"]["commit"],
        "accepted artifact is not the confirmed PR/batch member",
    )
}
