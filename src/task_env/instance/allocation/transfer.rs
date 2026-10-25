use super::*;
use crate::{
    lifecycle::{Owner, Permit},
    FlowError,
};
use std::{ffi::OsStr, fs::File, path::PathBuf};

impl Role {
    fn slot(self) -> &'static str {
        match self {
            Self::Workspace => "allocation-workspace",
            Self::Temporary => "allocation-temporary",
            Self::Checkout => "allocation-checkout",
        }
    }
    fn target(self, env: &TaskEnvironment) -> PathBuf {
        match self {
            Self::Workspace => env.workspace_root.clone(),
            Self::Temporary => env.temp_root.clone(),
            Self::Checkout => env.workspace_root.join("checkout"),
        }
    }
    fn identity(self, env: &TaskEnvironment) -> Option<RootIdentity> {
        match self {
            Self::Workspace => env.workspace_identity,
            Self::Temporary => env.temp_identity,
            Self::Checkout => env.creation.as_ref().unwrap().checkout_identity,
        }
    }
    fn set_identity(self, env: &mut TaskEnvironment, identity: RootIdentity) {
        match self {
            Self::Workspace => env.workspace_identity = Some(identity),
            Self::Temporary => env.temp_identity = Some(identity),
            Self::Checkout => env.creation.as_mut().unwrap().checkout_identity = Some(identity),
        }
    }
}

fn transfer(env: &TaskEnvironment) -> Result<&Transfer> {
    allocation(env)?
        .transfer
        .as_ref()
        .ok_or_else(|| FlowError::guard("allocation transfer missing"))
}
fn directory(env: &TaskEnvironment) -> Result<File> {
    allocation(env)?
        .envelope
        .as_ref()
        .ok_or_else(|| FlowError::guard("allocation envelope missing"))?
        .verify()
}
fn slot_path(env: &TaskEnvironment, role: Role) -> PathBuf {
    env.state_file().join(role.slot())
}
fn target_parent(env: &TaskEnvironment, role: Role) -> Result<fs::Directory> {
    let a = allocation(env)?;
    Ok(match role {
        Role::Workspace => a.workspace_parent.clone(),
        Role::Temporary => a.temp_parent.clone(),
        Role::Checkout => {
            let p = fs::Directory::capture(&env.workspace_root)?;
            require(
                Some(p.identity) == env.workspace_identity,
                "checkout parent identity changed",
            )?;
            p
        }
    })
}

fn verify_target(env: &TaskEnvironment, role: Role) -> Result<()> {
    let path = role.target(env);
    if let Some(expected) = role.identity(env) {
        verify_published(env, role, expected)
    } else {
        require(
            fs::absent(&path)?,
            "unrecorded target exists; refusing adoption",
        )
    }
}

fn verify_published(env: &TaskEnvironment, role: Role, expected: RootIdentity) -> Result<()> {
    let proof = fs::Directory::capture(&role.target(env))?;
    require(
        proof.identity == expected && proof.mount == allocation(env)?.parent.mount,
        "allocation target identity or mount changed",
    )
}

pub(crate) fn verify_recorded(env: &TaskEnvironment) -> Result<()> {
    verify(env)?;
    for role in [Role::Workspace, Role::Temporary, Role::Checkout] {
        if allocation(env)?
            .transfer
            .as_ref()
            .is_some_and(|t| t.role == role)
        {
            verify_transfer(env)?;
        } else {
            verify_target(env, role)?;
            require(
                fs::absent(&slot_path(env, role))?,
                "slot exists without transfer intent",
            )?;
        }
    }
    Ok(())
}

fn verify_transfer(env: &TaskEnvironment) -> Result<()> {
    let t = transfer(env)?;
    verify_transfer_parent(env, t)?;
    let staged = !fs::absent(&slot_path(env, t.role))?;
    let published = !fs::absent(&t.role.target(env))?;
    require(
        !(staged && published),
        "allocation slot and target both exist",
    )?;
    verify_location(env, t, staged, published)
}
fn verify_transfer_parent(env: &TaskEnvironment, t: &Transfer) -> Result<()> {
    require(
        t.target_parent.path == t.role.target(env).parent().unwrap(),
        "transfer target parent changed",
    )?;
    require(
        t.target_parent == target_parent(env, t.role)?,
        "transfer parent proof changed",
    )?;
    t.target_parent.verify()?;
    Ok(())
}
fn verify_location(
    env: &TaskEnvironment,
    t: &Transfer,
    staged: bool,
    published: bool,
) -> Result<()> {
    if published {
        let id = t
            .identity
            .ok_or_else(|| FlowError::guard("target has no durable transfer identity"))?;
        verify_published(env, t.role, id)?;
    } else if staged {
        verify_slot(env, t)?;
    } else {
        require(
            t.identity.is_none() || t.discarding,
            "recorded allocation disappeared",
        )?;
    }
    Ok(())
}

fn verify_slot(env: &TaskEnvironment, t: &Transfer) -> Result<RootIdentity> {
    let parent = directory(env)?;
    let child = fs::open_child(&parent, OsStr::new(t.role.slot()))?;
    fs::entries(&child, &[])?;
    let id = RootIdentity::of(&child.metadata()?);
    if let Some(expected) = t.identity {
        require(id == expected, "allocation slot identity changed")?;
    }
    Ok(id)
}

pub(crate) fn materialize(env: &mut TaskEnvironment) -> Result<()> {
    verify_recorded(env)?;
    for role in [Role::Workspace, Role::Temporary, Role::Checkout] {
        if role.identity(env).is_none() {
            materialize_role(env, role)?;
        }
    }
    verify_recorded(env)
}

fn materialize_role(env: &mut TaskEnvironment, role: Role) -> Result<()> {
    if allocation(env)?.transfer.is_none() {
        begin_transfer(env, role)?;
    }
    let current = transfer(env)?;
    require(
        current.role == role && !current.discarding,
        "allocation is being discarded",
    )?;
    allocate(env)?;
    publish(env)
}
fn begin_transfer(env: &mut TaskEnvironment, role: Role) -> Result<()> {
    let parent = target_parent(env, role)?;
    require(
        parent.mount == allocation(env)?.parent.mount,
        "allocation transfer crosses mounts",
    )?;
    env.creation
        .as_mut()
        .unwrap()
        .allocation
        .as_mut()
        .unwrap()
        .transfer = Some(Transfer {
        role,
        target_parent: parent,
        identity: None,
        discarding: false,
    });
    env.persist()?;
    #[cfg(test)]
    super::tests::checkpoint(&format!("intent:{}", role.slot()));
    Ok(())
}

fn allocate(env: &mut TaskEnvironment) -> Result<()> {
    if transfer(env)?.identity.is_some() {
        return verify_transfer(env);
    }
    let t = transfer(env)?.clone();
    let (_parent, _child, identity) = allocate_empty_slot(env, &t)?;
    env.creation
        .as_mut()
        .unwrap()
        .allocation
        .as_mut()
        .unwrap()
        .transfer
        .as_mut()
        .unwrap()
        .identity = Some(identity);
    env.persist()?;
    #[cfg(test)]
    super::tests::checkpoint(&format!("identity:{}", t.role.slot()));
    Ok(())
}

fn allocate_empty_slot(env: &TaskEnvironment, t: &Transfer) -> Result<(File, File, RootIdentity)> {
    require(
        fs::absent(&t.role.target(env))?,
        "allocation target already exists",
    )?;
    let parent = directory(env)?;
    let child = fs::reserved_dir(&parent, OsStr::new(t.role.slot()))?;
    fs::entries(&child, &[])?;
    let identity = RootIdentity::of(&child.metadata()?);
    Ok((parent, child, identity))
}

fn publish(env: &mut TaskEnvironment) -> Result<()> {
    verify_transfer(env)?;
    let t = transfer(env)?.clone();
    let id = t
        .identity
        .ok_or_else(|| FlowError::guard("transfer identity absent"))?;
    let source = directory(env)?;
    let target = t.target_parent.verify()?;
    rename_staged(env, &t, &source, &target)?;
    // A successful rename is not a durability checkpoint. Both parents must
    // sync, including when reconciling a lost reply, before publishing mapping.
    complete_handoff(env, &t, id, &source, &target)?;
    #[cfg(test)]
    super::tests::checkpoint(&format!("synced:{}", t.role.slot()));
    t.role.set_identity(env, id);
    env.creation
        .as_mut()
        .unwrap()
        .allocation
        .as_mut()
        .unwrap()
        .transfer = None;
    env.persist()?;
    #[cfg(test)]
    super::tests::checkpoint(&format!("published:{}", t.role.slot()));
    Ok(())
}

fn rename_staged(env: &TaskEnvironment, t: &Transfer, source: &File, target: &File) -> Result<()> {
    let path = t.role.target(env);
    if !fs::absent(&slot_path(env, t.role))? {
        verify_slot(env, t)?;
        #[cfg(test)]
        super::tests::checkpoint(&format!("before-transfer:{}", t.role.slot()));
        fs::rename(
            source,
            OsStr::new(t.role.slot()),
            target,
            path.file_name().unwrap(),
        )?;
        #[cfg(test)]
        super::tests::checkpoint(&format!("transferred:{}", t.role.slot()));
    }
    Ok(())
}

fn complete_handoff(
    env: &TaskEnvironment,
    t: &Transfer,
    id: RootIdentity,
    source: &File,
    target: &File,
) -> Result<()> {
    fs::sync_pair(source, target)?;
    t.target_parent.verify()?;
    verify_published(env, t.role, id)
}

pub(crate) fn settle_terminal(
    env: &mut TaskEnvironment,
    owner: &Owner,
    permit: &Permit,
) -> Result<()> {
    if super::super::intent(env)?.allocation.is_none() {
        return Ok(());
    }
    require_terminal(env, owner, permit)?;
    verify(env)?;
    if allocation(env)?.transfer.is_some() {
        verify_recorded(env)?;
        settle_transfer(env)?;
    }
    // With no unfinished transfer, leave root verification to the existing
    // frozen terminal plan. No root identity is inferred or changed here.
    discard_scratch(env)
}

fn require_terminal(_env: &TaskEnvironment, owner: &Owner, permit: &Permit) -> Result<()> {
    require(
        permit.exclusive() && permit.owner() == owner,
        "allocation cleanup requires terminal EX",
    )?;
    let q = crate::queue::load_queue(&owner.queue_root)?;
    permit.check(&q, true)?;
    require(
        q["tasks"][&owner.task_id]["cleanup_obligation"]["owner"] == serde_json::to_value(owner)?,
        "allocation cleanup has no terminal obligation",
    )?;
    Ok(())
}

fn settle_transfer(env: &mut TaskEnvironment) -> Result<()> {
    let t = transfer(env)?.clone();
    if !fs::absent(&t.role.target(env))? {
        settle_published(env, &t)?;
    } else if !fs::absent(&slot_path(env, t.role))? {
        discard_slot(env)?;
    }
    env.creation
        .as_mut()
        .unwrap()
        .allocation
        .as_mut()
        .unwrap()
        .transfer = None;
    // Only allocation metadata is reconciled under EX before freezing the
    // terminal snapshot. No business root or Git worktree is materialized.
    super::persist(env)
}

fn settle_published(env: &mut TaskEnvironment, t: &Transfer) -> Result<()> {
    let id = t
        .identity
        .ok_or_else(|| FlowError::guard("published identity missing"))?;
    let source = directory(env)?;
    let target = t.target_parent.verify()?;
    complete_handoff(env, t, id, &source, &target)?;
    t.role.set_identity(env, id);
    Ok(())
}

fn discard_slot(env: &mut TaskEnvironment) -> Result<()> {
    let t = transfer(env)?.clone();
    let identity = verify_slot(env, &t)?;
    let pending = env
        .creation
        .as_mut()
        .unwrap()
        .allocation
        .as_mut()
        .unwrap()
        .transfer
        .as_mut()
        .unwrap();
    pending.identity = Some(identity);
    pending.discarding = true;
    super::persist(env)?;
    fs::remove_empty(&directory(env)?, OsStr::new(t.role.slot()), identity)
}

fn discard_scratch(env: &TaskEnvironment) -> Result<()> {
    let dir = directory(env)?;
    let path = fs::anchored(&dir, OsStr::new(".allocation-next"))?;
    if !fs::absent(&path)? {
        require(
            std::fs::symlink_metadata(&path)?.is_file(),
            "allocation scratch replaced",
        )?;
        std::fs::remove_file(path)?;
        dir.sync_all()?;
    }
    Ok(())
}
