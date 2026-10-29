//! Cache derived fences only. Atomic authority replacement invalidates even
//! when its byte length and mtime are unchanged; failures never reuse old data.
use super::*;
use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};

#[derive(PartialEq, Eq)]
struct Stamp {
    identity: (u64, u64, u64),
    modified: (i64, i64),
    changed: (i64, i64),
    access: (u32, u32),
}

struct Entry {
    root: PathBuf,
    stamp: Stamp,
    fences: Arc<Fences>,
}

static CACHE: OnceLock<Mutex<VecDeque<Entry>>> = OnceLock::new();

fn stamp(root: &Path) -> Result<Stamp> {
    ensure(
        queue::isolation::isolated(root)?,
        "human admission requires an isolated queue",
    )?;
    let path = util::protected::read_path(&root.join("queue.json"))?;
    let m = std::fs::symlink_metadata(path)?;
    ensure(m.is_file(), "queue payload must be a regular file")?;
    Ok(Stamp {
        identity: (m.dev(), m.ino(), m.len()),
        modified: (m.mtime(), m.mtime_nsec()),
        changed: (m.ctime(), m.ctime_nsec()),
        access: (m.uid(), m.mode()),
    })
}

pub(super) fn load(root: &Path) -> Result<Arc<Fences>> {
    let before = stamp(root)?;
    let mut entries = CACHE
        .get_or_init(Mutex::default)
        .lock()
        .map_err(|_| FlowError::guard("human admission cache unavailable"))?;
    if let Some(entry) = entries.iter().find(|e| e.root == root && e.stamp == before) {
        return Ok(entry.fences.clone());
    }
    let fences = Arc::new(fences(&queue::load_queue(root)?));
    ensure(before == stamp(root)?, "queue changed during admission")?;
    entries.retain(|entry| entry.root != root);
    if entries.len() == 16 {
        entries.pop_front();
    }
    entries.push_back(Entry {
        root: root.to_path_buf(),
        stamp: before,
        fences: fences.clone(),
    });
    Ok(fences)
}

#[cfg(test)]
#[path = "../../../../tests/common/human_gate_cache.rs"]
mod tests;
