//! Short-lived task-instance mutation permits. No scheduler or cleanup executor.
use crate::{model::J, util, FlowError, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    fs::{self, File, OpenOptions},
    os::{
        fd::AsRawFd,
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::PathBuf,
};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Owner {
    pub queue_root: PathBuf,
    pub queue_id: String,
    pub task_id: String,
    pub epoch: u64,
    pub instance_id: String,
}

#[derive(Debug)]
pub struct Permit {
    owner: Owner,
    file: File,
    path: PathBuf,
    exclusive: bool,
}

pub fn acquire(owner: &Owner, exclusive: bool) -> Result<Permit> {
    acquire_with_creation(owner, exclusive, exclusive && owner.instance_id.is_empty())
}

fn acquire_with_creation(owner: &Owner, exclusive: bool, create: bool) -> Result<Permit> {
    validate_owner_location(owner)?;
    let root = crate::queue::artifacts(&owner.queue_root)?.join("gates");
    util::ensure_private_dir(&root, false)?;
    let path = root.join(format!("{}.lock", owner.task_id));
    let file = OpenOptions::new()
        .read(true)
        .write(exclusive)
        .create(create)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&path)?;
    let mode = if exclusive {
        libc::LOCK_EX
    } else {
        libc::LOCK_SH
    };
    lock_permit(&file, mode)?;
    let permit = Permit {
        owner: owner.clone(),
        file,
        path,
        exclusive,
    };
    permit.identity()?;
    Ok(permit)
}
fn validate_owner_location(owner: &Owner) -> Result<()> {
    crate::task_env::validate_task_id(&owner.task_id)?;
    if !owner.queue_root.is_absolute() || owner.epoch == 0 || owner.queue_id.is_empty() {
        return Err(FlowError::guard("invalid lifecycle owner"));
    }
    if !util::protected::is_document(&owner.queue_root.join("queue.json"))? {
        return Err(FlowError::guard(
            "lifecycle requires an isolated queue authority",
        ));
    }
    Ok(())
}
impl Permit {
    fn identity(&self) -> Result<()> {
        let held = self.file.metadata()?;
        let named = fs::symlink_metadata(&self.path)?;
        if !named.is_file()
            || named.uid() != util::process::current_uid()
            || named.mode() & 0o077 != 0
            || (held.dev(), held.ino()) != (named.dev(), named.ino())
        {
            return Err(FlowError::guard("lifecycle gate identity changed"));
        }
        Ok(())
    }
    pub fn owner(&self) -> &Owner {
        &self.owner
    }
    pub fn exclusive(&self) -> bool {
        self.exclusive
    }
    pub fn check(&self, s: &J, allow_terminal: bool) -> Result<()> {
        self.identity()?;
        let o = &self.owner;
        let t = &s["tasks"][&o.task_id];
        let m = &t["environment"];
        verify_task_tuple(s, t, o)?;
        verify_mapping(m, o)?;
        verify_action_phase(t, allow_terminal)?;
        if allow_terminal && !self.exclusive {
            return Err(FlowError::guard(
                "terminal requires exclusive lifecycle permit",
            ));
        }
        Ok(())
    }
}

fn verify_action_phase(t: &J, allow_terminal: bool) -> Result<()> {
    if !allow_terminal
        && (!t["final_outcome"].is_null()
            || !t["cleanup_obligation"].is_null()
            || t["environment"]["phase"] != "active")
    {
        return Err(FlowError::guard("task instance is closing or not active"));
    }
    Ok(())
}

fn verify_task_tuple(s: &J, t: &J, o: &Owner) -> Result<()> {
    if s["lifecycle"]["queue_root"] != json!(o.queue_root)
        || s["id"] != o.queue_id
        || t["task"] != o.task_id
        || t["epoch"] != o.epoch
    {
        return Err(FlowError::guard("lifecycle owner tuple changed"));
    }
    Ok(())
}
fn verify_mapping(m: &J, o: &Owner) -> Result<()> {
    if m["queue_id"] != o.queue_id
        || m["task_id"] != o.task_id
        || m["epoch"] != o.epoch
        || m["instance_id"] != o.instance_id
        || o.instance_id.is_empty()
    {
        return Err(FlowError::guard("lifecycle owner tuple changed"));
    }
    Ok(())
}

pub fn write_permit(owner: &Owner) -> Result<Permit> {
    let permit = acquire(owner, false)?;
    let state = crate::queue::load_queue(&owner.queue_root)?;
    permit.check(&state, false)?;
    Ok(permit)
}

/// Resolve only an explicit lifecycle owner or its declared lineage parent.
pub fn task_owner(s: &J, name: &str) -> Result<Option<Owner>> {
    let task = &s["tasks"][name];
    let owner_name = task["lineage"]["requirement"].as_str().unwrap_or(name);
    let t = &s["tasks"][owner_name];
    let m = &t["environment"];
    if m.is_null() {
        return Ok(None);
    }
    let root = s["lifecycle"]["queue_root"]
        .as_str()
        .ok_or_else(|| FlowError::guard("missing isolated queue root"))?;
    let owner: Owner = serde_json::from_value(
        json!({"queue_root":root,"queue_id":s["id"],"task_id":owner_name,"epoch":t["epoch"],"instance_id":m["instance_id"]}),
    )?;
    Ok(Some(owner))
}

/// Runtime/receiver journals inherit an explicit owner; no discovery by path/name.
pub fn runtime_permit(root: &std::path::Path) -> Result<Option<Permit>> {
    let marker = root.join("lifecycle-owner.json");
    match fs::symlink_metadata(&marker) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if util::protected::is_runtime_namespace(root)? {
                return Err(FlowError::guard("isolated runtime owner is missing"));
            }
            Ok(None)
        }
        Err(e) => Err(e.into()),
        Ok(_) => acquire_runtime(root, &marker),
    }
}

fn acquire_runtime(root: &std::path::Path, marker: &std::path::Path) -> Result<Option<Permit>> {
    if !util::protected::is_runtime_namespace(root)? {
        return Err(FlowError::guard("runtime authority namespace is missing"));
    }
    if !util::protected::is_document(marker)? {
        return Err(FlowError::guard("runtime owner must be isolated authority"));
    }
    let owner: Owner =
        serde_json::from_value(util::read_state_json(root, "lifecycle-owner.json")?)?;
    let permit = write_permit(&owner)?;
    verify_runtime_record(root, &owner)?;
    crate::task_runtime::retirement::admit(root)?;
    Ok(Some(permit))
}

/// Stored submission identity, never reconstructed from a later queue snapshot.
pub fn context_permit(context: &J) -> Result<Option<Permit>> {
    let Some(value) = context.get("lifecycle_owner").filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let owner: Owner = serde_json::from_value(value.clone())?;
    write_permit(&owner).map(Some)
}

fn verify_runtime_record(root: &std::path::Path, owner: &Owner) -> Result<()> {
    if root.join("runtime.json").exists() {
        let record = util::read_state_json(root, "runtime.json")?;
        if record["instance"] != owner.instance_id
            || record["lifecycle_owner"] != serde_json::to_value(owner)?
        {
            return Err(FlowError::guard("runtime lifecycle record changed"));
        }
    }
    Ok(())
}

/// Caller holds queue lock and has re-read the exact current terminal owner.
/// Only a released tail can recreate a gate lost after its retirement.
pub fn acquire_terminal(owner: &Owner) -> Result<Permit> {
    let s = crate::queue::load_queue(&owner.queue_root)?;
    let t = &s["tasks"][&owner.task_id];
    verify_task_tuple(&s, t, owner)?;
    verify_mapping(&t["environment"], owner)?;
    if t["cleanup_obligation"]["owner"] != json!(owner)
        || !matches!(t["final_outcome"].as_str(), Some("success" | "failure"))
    {
        return Err(FlowError::guard(
            "terminal gate requires current cleanup obligation",
        ));
    }
    let recreate = t["cleanup_obligation"]["status"] == "released"
        && !t["cleanup_obligation"]["plan_removal"].is_null();
    acquire_with_creation(owner, true, recreate)
}

/// Retire only the held gate under the queue transaction, before removing owner.
/// Old descriptors cannot pass identity() after unlink or replacement.
pub fn retire(owner: &Owner, permit: &Permit) -> Result<()> {
    verify_retirement(owner, permit)?;
    permit.identity()?;
    fs::remove_file(&permit.path)?;
    File::open(permit.path.parent().unwrap())?.sync_all()?;
    Ok(())
}

fn verify_retirement(owner: &Owner, permit: &Permit) -> Result<()> {
    let s = crate::queue::load_queue(&owner.queue_root)?;
    permit.check(&s, true)?;
    if !permit.exclusive
        || permit.owner() != owner
        || s["tasks"][&owner.task_id]["cleanup_obligation"]["status"] != "released"
    {
        return Err(FlowError::guard(
            "gate retirement requires released current owner",
        ));
    }
    Ok(())
}

fn lock_permit(file: &std::fs::File, mode: i32) -> Result<()> {
    // SAFETY: a valid held fd and scalar operation; always nonblocking.
    if unsafe { libc::flock(file.as_raw_fd(), mode | libc::LOCK_NB) } == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.kind() == std::io::ErrorKind::WouldBlock {
        return Err(FlowError::new(
            crate::ErrorKind::StateBusy,
            "lifecycle action busy; release other locks and retry",
        ));
    }
    Err(error.into())
}
