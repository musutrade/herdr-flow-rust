//! Resume only components proved before GitRemoving. Never repository-wide prune.
use super::*;
use crate::task_env::instance::git as identity;

pub(super) fn remove(plan: &mut CleanupState) -> Result<()> {
    let intent = removal_intent(plan)?;
    let Some(g) = &intent.git_identity else {
        return identity::verify_removal(&intent);
    };
    identity::verify_source(&intent)?;
    let root = PendingDirectory {
        path: g.admin_dir.clone(),
        identity: Some(g.admin_identity),
    };
    if resume_admin_quarantine(plan, &intent, &root)? {
        return Ok(());
    }
    remove_remaining_admin(plan, &intent, &root)
}

fn remove_remaining_admin(
    plan: &mut CleanupState,
    intent: &CreationIntent,
    root: &PendingDirectory,
) -> Result<()> {
    let state = identity::removal_presence(intent)?;
    if !state.admin {
        return identity::verify_unregistered(intent);
    }
    if state.complete_admin && (!state.checkout || state.git_file) {
        return identity::remove_registered(intent, &terminal(plan)?.owner);
    }
    // Git may have partly erased its admin, or the checkout .git is gone. The
    // persisted inode/backlink/common identity grants removal of this single
    // remaining admin directory. A quarantine checkpoint precedes its contents.
    storage::reject_mounts(&root.path)?;
    super::super::quarantine_before_clear(plan, root)?;
    identity::verify_unregistered(intent)
}

fn resume_admin_quarantine(
    plan: &mut CleanupState,
    intent: &CreationIntent,
    root: &PendingDirectory,
) -> Result<bool> {
    if !plan
        .quarantine
        .as_ref()
        .is_some_and(|q| q.root == root.path)
    {
        return Ok(false);
    }
    super::super::cleanup_one_root(plan, root)?;
    plan.quarantine = None;
    storage::save(plan)?;
    identity::verify_unregistered(intent)?;
    Ok(true)
}

/// Freeze recovered Git identity separately; the environment snapshot is immutable.
pub(super) fn prepare_removal(plan: &mut CleanupState) -> Result<()> {
    let mut intent = terminal(plan)?.creation.clone();
    let recovered = identity::pending_removal_identity(&intent)?;
    if let Some(proof) = &recovered {
        require(
            plan.roots.iter().all(|root| root.identity.is_some()),
            "pending Git removal requires recorded root identities",
        )?;
        intent.git_identity = Some(proof.clone());
    }
    identity::verify_removal(&intent)?;
    plan.terminal.as_mut().unwrap().git_removal_identity = recovered;
    Ok(())
}

pub(super) fn removal_intent(plan: &CleanupState) -> Result<CreationIntent> {
    let t = terminal(plan)?;
    let mut intent = t.creation.clone();
    if let Some(proof) = &t.git_removal_identity {
        require(
            plan.roots.iter().all(|root| root.identity.is_some()),
            "pending Git removal requires recorded root identities",
        )?;
        require(
            t.phase != Phase::Prepared,
            "removal identity has no GitRemoving checkpoint",
        )?;
        identity::validate_pending_removal_identity(&intent, proof)?;
        intent.git_identity = Some(proof.clone());
    }
    Ok(intent)
}
