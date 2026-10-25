use super::*;
use crate::task_env::{DeliveryTarget, TaskEnvironment};
use std::{fs, path::PathBuf};

const LIMIT: usize = 8 * 1024 * 1024;
fn base() -> Result<Option<PathBuf>> {
    let root = registry::control_path()?;
    match fs::symlink_metadata(&root) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
        Ok(_) => (),
    }
    util::ensure_private_dir(&root, false)?;
    isolated_base(&root).map(Some)
}

pub(super) fn plan_path(owner: &Owner) -> Result<PathBuf> {
    registry::validate_task_id(&owner.task_id)?;
    let base = base()?.ok_or_else(|| FlowError::guard("cleanup control absent"))?;
    let binding = util::read_state_json(&base, "queue-binding.json")?;
    require(
        binding == serde_json::json!({"queue_root":owner.queue_root,"queue_id":owner.queue_id}),
        "registry queue binding changed",
    )?;
    Ok(base.join("plans").join(format!("{}.json", owner.task_id)))
}
pub(super) fn load_optional(owner: &Owner) -> Result<Option<CleanupState>> {
    let path = plan_path(owner)?;
    if !path.parent().unwrap().try_exists()? {
        return Ok(None);
    }
    let value = util::read_state_json_bounded_if_exists(
        path.parent().unwrap(),
        path.file_name().unwrap().to_str().unwrap(),
        LIMIT,
    )?;
    value.map(|v| decode(v, owner)).transpose()
}
fn decode(value: J, owner: &Owner) -> Result<CleanupState> {
    let plan: CleanupState = serde_json::from_value(value)?;
    validate_plan_owner(&plan, owner)?;
    validate_roots(&plan.roots, &[])?;
    require(
        plan.pending_root_removals
            .iter()
            .all(|p| plan.roots.iter().any(|r| &r.path == p)),
        "foreign pending root",
    )?;
    validate_quarantine(&plan)?;
    Ok(plan)
}
fn validate_quarantine(plan: &CleanupState) -> Result<()> {
    let Some(q) = &plan.quarantine else {
        return Ok(());
    };
    require(
        q.name.starts_with(".herdr-rm-")
            && Path::new(&q.name).components().count() == 1
            && matches!(
                Path::new(&q.name).components().next(),
                Some(std::path::Component::Normal(_))
            ),
        "invalid terminal quarantine name",
    )?;
    let t = terminal(plan)?;
    let owned_root = plan.pending_root_removals.contains(&q.root);
    let owned_admin = t.phase == Phase::GitRemoving
        && t.creation
            .git_identity
            .as_ref()
            .is_some_and(|g| g.admin_dir == q.root);
    require(
        owned_root || owned_admin,
        "foreign terminal quarantine root",
    )
}
pub(super) fn load(owner: &Owner) -> Result<CleanupState> {
    load_optional(owner)?.ok_or_else(|| FlowError::guard("terminal plan missing"))
}
pub(super) fn save(plan: &CleanupState) -> Result<()> {
    let path = plan_path(&terminal(plan)?.owner)?;
    util::atomic_write_json(&path, &serde_json::to_value(plan)?)
}
pub(super) fn validate_roots(roots: &[PendingDirectory], targets: &[DeliveryTarget]) -> Result<()> {
    require(
        roots.len() == 2,
        "terminal requires exact workspace/temp roots",
    )?;
    let control = util::resolve_path(&registry::control_path()?);
    for root in roots {
        require(
            root.path.is_absolute() && !util::overlap(&root.path, &control),
            "cleanup root overlaps control",
        )?;
        super::super::ensure_outside_deliveries(&root.path, targets)?;
    }
    require(
        !util::overlap(&roots[0].path, &roots[1].path),
        "cleanup roots overlap",
    )
}
pub(super) fn validate_entry(
    env: &TaskEnvironment,
    entry: Option<&registry::RegistryEntry>,
) -> Result<()> {
    if let Some(e) = entry {
        let mut actual = e.claimed_roots();
        actual.sort();
        let mut expected = env.roots().into_iter().map(|(p, _)| p).collect::<Vec<_>>();
        expected.sort();
        require(
            entry_identity(e, env) && actual == expected,
            "terminal registry claims changed",
        )?;
    } else {
        require(
            env.state == super::super::super::EnvironmentState::Creating,
            "active registry claim missing",
        )?;
    }
    Ok(())
}
fn absent(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(false),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(e) => Err(e.into()),
    }
}
pub(super) fn verify_pending_roots(plan: &CleanupState) -> Result<()> {
    validate_quarantine(plan)?;
    if let Some(q) = &plan.quarantine {
        reject_mounts(
            &q.root
                .parent()
                .ok_or_else(|| FlowError::guard("quarantine parent missing"))?
                .join(&q.name),
        )?;
    }
    for root in &plan.roots {
        verify_pending_root(plan, root)?;
    }
    Ok(())
}

pub(super) fn remove_roots(plan: &mut CleanupState) -> Result<()> {
    while let Some(path) = plan.pending_root_removals.first().cloned() {
        let root = super::super::find_root(plan, &path)?;
        if root.identity.is_none() {
            require(absent(&path)?, "unknown partial root retained")?;
        } else {
            reject_mounts(&path)?;
            super::super::cleanup_one_root(plan, &root)?;
        }
        plan.pending_root_removals.remove(0);
        plan.quarantine = None;
        save(plan)?;
    }
    Ok(())
}
pub(super) fn reject_mounts(path: &Path) -> Result<()> {
    // Task instances do not grant mount ownership. Never unmount an arbitrary
    // mounted tree or walk through it while deleting an owned root.
    let mounts = fs::read_to_string("/proc/self/mountinfo")?;
    for row in mounts.lines() {
        let mount = row
            .split_whitespace()
            .nth(4)
            .ok_or_else(|| FlowError::guard("malformed mount observation"))?;
        let mount = mount
            .replace("\\040", " ")
            .replace("\\011", "\t")
            .replace("\\012", "\n")
            .replace("\\134", "\\");
        require(
            !Path::new(&mount).starts_with(path),
            "mounted subtree has no task ownership; retry after managed unmount",
        )?;
    }
    Ok(())
}
pub(super) fn verify_absent(plan: &CleanupState) -> Result<()> {
    for root in &plan.roots {
        require(absent(&root.path)?, "task root still present")?;
    }
    if let Some(q) = &plan.quarantine {
        let path = q.root.parent().unwrap().join(&q.name);
        require(absent(&path)?, "quarantine remains")?;
    }
    super::super::super::instance::git::verify_removed(&terminal(plan)?.creation)?;
    process::ensure_gone(plan)
}

pub(super) fn pending(queue_root: &Path) -> Result<Vec<Owner>> {
    let Some(base) = base()? else {
        return Ok(vec![]);
    };
    let q = bound_queue(queue_root, &base)?;
    let dir = base.join("plans");
    let entries = match fs::read_dir(&dir) {
        Ok(v) => v,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(e.into()),
    };
    require(
        util::protected::is_namespace(&dir)?,
        "plans namespace missing",
    )?;
    scan_plan_entries(&q, &dir, entries)
}

fn released_proof(q: &J, path: &Path) -> Result<bool> {
    for task in q["tasks"]
        .as_object()
        .ok_or_else(|| FlowError::guard("invalid queue tasks"))?
        .values()
    {
        let o = &task["cleanup_obligation"];
        if o["status"] != "released" {
            continue;
        }
        let proof: DocumentRemoval = serde_json::from_value(o["plan_removal"].clone())?;
        if proof.path == path {
            verify_released_proof(q, o, &proof, path)?;
            return Ok(true);
        }
    }
    Ok(false)
}

pub(super) fn verify_environment(plan: &CleanupState) -> Result<()> {
    let t = terminal(plan)?;
    let doc = read_environment_snapshot(t)?;
    let env: TaskEnvironment = serde_json::from_value(doc)?;
    require(
        crate::task_env::isolation::owner(&env).as_ref() == Some(&t.owner)
            && env.state_file() == t.environment.path,
        "environment snapshot owner changed",
    )?;
    verify_snapshot_resources(plan, &env, t)
}

pub(super) fn validate_receiver(
    env: &TaskEnvironment,
    associations: &J,
    owner: &Owner,
) -> Result<()> {
    if associations["owned_receiver"] != true {
        return Ok(());
    }
    require(
        associations["receiver_owner"] == serde_json::to_value(owner)?,
        "owned receiver tuple missing",
    )?;
    let path = associations["guard_dir"]
        .as_str()
        .map(Path::new)
        .ok_or_else(|| FlowError::guard("owned receiver path missing"))?;
    require(
        path.is_absolute() && env.roots().iter().any(|(root, _)| path.starts_with(root)),
        "owned receiver outside registered roots",
    )?;
    verify_receiver_path(path, owner)
}

fn isolated_base(root: &Path) -> Result<PathBuf> {
    let doc = root.join("registry.json");
    if !util::protected::is_document(&doc)? {
        return Err(FlowError::guard(
            "terminal cleanup requires isolated registry",
        ));
    }
    require(
        util::protected::is_namespace(&doc)?,
        "registry namespace missing",
    )?;
    Ok(util::resolve_path(&doc))
}
fn validate_plan_owner(plan: &CleanupState, owner: &Owner) -> Result<()> {
    require(
        plan.task_id == owner.task_id
            && plan.instance_id == owner.instance_id
            && terminal(plan)?.owner == *owner,
        "cleanup plan owner changed",
    )?;
    validate_creation_owner(&terminal(plan)?.creation, owner)
}

fn validate_creation_owner(creation: &CreationIntent, owner: &Owner) -> Result<()> {
    require(
        creation.lifecycle_queue.as_ref() == Some(&owner.queue_root)
            && creation.binding.task_id == owner.task_id
            && creation.binding.queue_id == owner.queue_id
            && creation.binding.epoch == owner.epoch,
        "cleanup creation binding changed",
    )?;
    Ok(())
}

fn entry_identity(e: &registry::RegistryEntry, env: &TaskEnvironment) -> bool {
    e.task_id == env.task_id
        && e.instance_id == env.instance_id
        && e.state_file == env.state_file()
        && e.workspace_root == env.workspace_root
        && e.temp_root == env.temp_root
}

fn verify_pending_root(plan: &CleanupState, root: &PendingDirectory) -> Result<()> {
    reject_mounts(&root.path)?;
    if !plan.pending_root_removals.contains(&root.path) {
        require(absent(&root.path)?, "removed root reappeared")?;
        return Ok(());
    }
    // An existing quarantine is verified by the descriptor cleanup engine.
    if plan
        .quarantine
        .as_ref()
        .is_some_and(|q| q.root == root.path)
    {
        return Ok(());
    }
    verify_root_identity(root)
}

fn bound_queue(queue_root: &Path, base: &Path) -> Result<J> {
    let q = crate::queue::load_queue(queue_root)?;
    let binding = util::read_state_json(base, "queue-binding.json")?;
    require(
        binding
            == serde_json::json!({"queue_root":util::resolve_path(queue_root),"queue_id":q["id"]}),
        "cleanup registry queue mismatch",
    )?;
    Ok(q)
}

fn pending_entry(q: &J, dir: &Path, entry: &fs::DirEntry) -> Result<Option<Owner>> {
    let path = entry.path();
    require(
        path.extension().is_some_and(|e| e == "json"),
        "unknown cleanup plan entry",
    )?;
    if released_proof(q, &path)? {
        return Ok(None);
    }
    let owner = read_plan_entry(dir, entry, &path)?;
    Ok(Some(owner))
}

fn read_plan_entry(dir: &Path, entry: &fs::DirEntry, path: &Path) -> Result<Owner> {
    let name = entry
        .file_name()
        .into_string()
        .map_err(|_| FlowError::guard("invalid plan name"))?;
    let value = util::read_state_json_bounded(dir, &name, LIMIT)?;
    let raw: CleanupState = serde_json::from_value(value.clone())?;
    let owner = terminal(&raw)?.owner.clone();
    require(
        plan_path(&owner)? == path,
        "cleanup filename identity mismatch",
    )?;
    decode(value, &owner)?;
    Ok(owner)
}

fn verify_released_proof(q: &J, o: &J, proof: &DocumentRemoval, path: &Path) -> Result<()> {
    let owner: Owner = serde_json::from_value(o["owner"].clone())?;
    require(
        crate::lifecycle::task_owner(q, &owner.task_id)?.as_ref() == Some(&owner),
        "released plan owner changed",
    )?;
    use std::os::unix::fs::MetadataExt;
    let m = fs::symlink_metadata(path)?;
    require(
        m.is_dir() && (m.dev(), m.ino()) == (proof.dev, proof.ino),
        "released plan inode changed",
    )?;
    Ok(())
}

fn read_environment_snapshot(t: &TerminalState) -> Result<J> {
    use std::os::unix::fs::MetadataExt;
    let m = fs::symlink_metadata(&t.environment.path)?;
    require(
        m.is_dir() && (m.dev(), m.ino()) == (t.environment.dev, t.environment.ino),
        "environment authority replaced",
    )?;
    let path = &t.environment.path;
    let doc = util::read_state_json_bounded(
        path.parent().unwrap(),
        path.file_name().unwrap().to_str().unwrap(),
        LIMIT,
    )?;
    require(
        util::pyjson::sha256_json_sorted(&doc) == t.environment_digest,
        "environment changed after terminal snapshot",
    )?;
    Ok(doc)
}

fn verify_receiver_path(path: &Path, owner: &Owner) -> Result<()> {
    util::ensure_private_dir(path, false)?;
    require(
        util::resolve_path(path) == path && util::protected::is_runtime_namespace(path)?,
        "owned receiver path identity changed",
    )?;
    let stored: Owner =
        serde_json::from_value(util::read_state_json(path, "lifecycle-owner.json")?)?;
    require(&stored == owner, "owned receiver identity changed")
}

fn scan_plan_entries(q: &J, dir: &Path, entries: fs::ReadDir) -> Result<Vec<Owner>> {
    let mut owners = vec![];
    for (index, entry) in entries.enumerate() {
        require(index < 4096, "cleanup scan budget exceeded")?;
        let entry = entry?;
        if entry.file_name() == ".flow-namespace-v2" {
            continue;
        }
        if let Some(owner) = pending_entry(q, dir, &entry)? {
            owners.push(owner);
        }
    }
    Ok(owners)
}
fn verify_root_identity(root: &PendingDirectory) -> Result<()> {
    match root.identity {
        Some(id) => {
            let m = fs::symlink_metadata(&root.path)?;
            require(
                m.is_dir() && crate::task_env::RootIdentity::of(&m) == id,
                "root identity changed",
            )?;
        }
        None => require(
            absent(&root.path)?,
            "unknown partial root identity; no adoption",
        )?,
    }
    Ok(())
}

fn verify_snapshot_resources(
    plan: &CleanupState,
    env: &TaskEnvironment,
    t: &TerminalState,
) -> Result<()> {
    let roots = env
        .roots()
        .into_iter()
        .map(|(path, identity)| PendingDirectory { path, identity })
        .collect::<Vec<_>>();
    require(
        serde_json::to_value(roots)? == serde_json::to_value(&plan.roots)?
            && serde_json::to_value(&env.creation)? == serde_json::to_value(Some(&t.creation))?,
        "cleanup plan disagrees with environment snapshot",
    )
}
