//! Bounded, non-creating observations. No lifecycle path helpers or locks.
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::{util, FlowError, Result};
use serde_json::Value;

const DOCUMENT_LIMIT: usize = 1024 * 1024;
const SNAPSHOT_LIMIT: usize = 8 * DOCUMENT_LIMIT;
const ENTRY_LIMIT: usize = 1024;

#[derive(Debug, PartialEq)]
pub(super) struct Snapshot {
    pub registry: Option<Value>,
    pub envs: BTreeMap<String, Value>,
    pub plans: BTreeMap<String, Value>,
    directories: Vec<Option<(u64, u64)>>,
}

pub(super) fn capture(root: &Path) -> Result<Snapshot> {
    let directories = identities(root)?;
    let mut budget = SNAPSHOT_LIMIT;
    let snapshot = Snapshot {
        registry: document(root, "registry.json", &mut budget)?,
        envs: scan(&root.join("envs"), &mut budget)?,
        plans: scan(&root.join("plans"), &mut budget)?,
        directories,
    };
    if snapshot.directories != identities(root)? {
        return Err(FlowError::progress(
            "control directories changed during status",
        ));
    }
    Ok(snapshot)
}

fn identities(root: &Path) -> Result<Vec<Option<(u64, u64)>>> {
    [root.to_path_buf(), root.join("envs"), root.join("plans")]
        .iter()
        .map(|p| directory(p))
        .collect()
}

fn directory(path: &Path) -> Result<Option<(u64, u64)>> {
    // Check existing prefixes even when the requested leaf is absent. A dangling
    // link in a prefix must not become an "absent" result.
    for ancestor in path.ancestors().collect::<Vec<_>>().into_iter().rev() {
        match fs::symlink_metadata(ancestor) {
            Ok(m) if !m.is_dir() => {
                return Err(FlowError::guard("status requires real directories"))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
            _ => (),
        }
    }
    util::ensure_private_dir(path, false)?;
    let meta = fs::symlink_metadata(path)?;
    Ok(Some((meta.dev(), meta.ino())))
}

fn document(dir: &Path, name: &str, budget: &mut usize) -> Result<Option<Value>> {
    if directory(dir)?.is_none() {
        return Ok(None);
    }
    let value = util::read_state_json_bounded_if_exists(dir, name, DOCUMENT_LIMIT)
        .map_err(|e| FlowError::progress(format!("{}: {e}", dir.join(name).display())))?;
    if let Some(value) = &value {
        *budget = budget
            .checked_sub(serde_json::to_vec(value)?.len())
            .ok_or_else(|| FlowError::progress("status snapshot exceeds 8 MiB"))?;
    }
    Ok(value)
}

fn scan(dir: &Path, budget: &mut usize) -> Result<BTreeMap<String, Value>> {
    let mut result = BTreeMap::new();
    if directory(dir)?.is_none() {
        return Ok(result);
    }
    for (index, entry) in fs::read_dir(dir)?.enumerate() {
        if index >= ENTRY_LIMIT {
            return Err(FlowError::progress("status directory exceeds 1024 entries"));
        }
        add_entry(dir, entry?.file_name(), budget, &mut result)?;
    }
    Ok(result)
}

fn add_entry(
    dir: &Path,
    name: std::ffi::OsString,
    budget: &mut usize,
    result: &mut BTreeMap<String, Value>,
) -> Result<()> {
    if let Some(task) = entry_task(name)? {
        let value = document(dir, &format!("{task}.json"), budget)?
            .ok_or_else(|| FlowError::progress("status document disappeared during scan"))?;
        result.insert(task, value);
    }
    Ok(())
}

fn entry_task(name: std::ffi::OsString) -> Result<Option<String>> {
    let name = name
        .into_string()
        .map_err(|_| FlowError::progress("status directory has a non-UTF8 entry"))?;
    if PathBuf::from(&name)
        .extension()
        .is_some_and(|e| e == "json")
    {
        let task = name.strip_suffix(".json").unwrap();
        super::super::validate_task_id(task)?;
        Ok(Some(task.to_owned()))
    } else {
        Ok(None)
    }
}

#[cfg(test)]
pub(super) fn changed_snapshot(root: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let first = capture(root)?;
    fs::write(root.join("registry.json"), b"{\"entries\":{}}")?;
    fs::set_permissions(
        root.join("registry.json"),
        fs::Permissions::from_mode(0o600),
    )?;
    super::unchanged(&first, &capture(root)?)
}
