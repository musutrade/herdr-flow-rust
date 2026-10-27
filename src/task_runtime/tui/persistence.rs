//! Exact task-pane reclamation from immutable Herdr v3 recovery copies.
//! The running server owns session.json; we only wait for its close to persist.
use super::*;
use std::fs::{self, OpenOptions};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
mod snapshot;
#[cfg(test)]
mod tests;

const MAX_BYTES: u64 = 8 * 1024 * 1024;

pub(super) fn configure(config: &mut J) -> Result<()> {
    let Some(selected) = config.get("session_store") else {
        return Ok(());
    };
    let path = selected
        .as_str()
        .or_else(|| selected["path"].as_str())
        .ok_or_else(|| FlowError::guard("Herdr session_store path missing"))?;
    let identity = store_identity(Path::new(path))?;
    if selected.is_object() {
        ensure(
            *selected == identity,
            "Herdr session store directory changed",
        )?;
    }
    let (_, _, value) = read_snapshot(Path::new(path))?;
    snapshot::validate(&value)?;
    config["session_store"] = identity;
    Ok(())
}

fn store_identity(path: &Path) -> Result<J> {
    ensure(
        path.is_absolute() && path.file_name().is_some_and(|n| n == "session.json"),
        "Herdr session_store must name an absolute session.json",
    )?;
    ensure(
        fs::canonicalize(path)? == path,
        "Herdr session store cannot use symlinks",
    )?;
    let parent = path
        .parent()
        .ok_or_else(|| FlowError::guard("session store parent missing"))?;
    let metadata = store_directory(parent)?;
    Ok(json!({"path":path,"dev":metadata.dev(),"ino":metadata.ino(),"uid":metadata.uid()}))
}

fn store_directory(path: &Path) -> Result<fs::Metadata> {
    ensure(
        fs::canonicalize(path)? == path,
        "Herdr store directory cannot use symlinks",
    )?;
    let metadata = fs::symlink_metadata(path)?;
    // These are existing Herdr user-data directories, not Flow's private task
    // namespaces. Herdr commonly creates them as 0775; never chmod user paths.
    ensure(
        metadata.is_dir()
            && metadata.uid() == util::process::current_uid()
            && metadata.mode() & 0o002 == 0,
        "Herdr store directory ownership changed",
    )?;
    Ok(metadata)
}

pub(super) fn reclaim(binding: &J) -> Result<()> {
    let store = &binding["session_store"];
    if store.is_null() {
        return Ok(());
    } // Older bindings never guessed storage ownership.
    let path = PathBuf::from(required(store, "path")?);
    ensure(
        store_identity(&path)? == *store,
        "Herdr session store directory changed",
    )?;
    await_saved_close(&path, binding)?;
    let parent = path.parent().unwrap();
    for name in ["session-snapshots", "session-backups"] {
        reclaim_directory(&parent.join(name), binding)?;
    }
    Ok(())
}

pub(super) fn capture(binding: &J) -> Result<J> {
    if binding["session_store"].is_null() || binding["persisted_panes"].is_array() {
        return Ok(binding.clone());
    }
    let path = PathBuf::from(required(&binding["session_store"], "path")?);
    ensure(
        store_identity(&path)? == binding["session_store"],
        "Herdr session store directory changed",
    )?;
    let mut captured = binding.clone();
    captured["persisted_panes"] = json!(await_identity(&path, binding)?);
    Ok(captured)
}

fn await_identity(path: &Path, binding: &J) -> Result<Vec<J>> {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let (_, _, saved) = read_snapshot(path)?;
        let ids = snapshot::identify(&saved, binding)?;
        if !ids.is_empty() {
            return Ok(ids);
        }
        ensure(
            Instant::now() < deadline,
            "Herdr task pane persistence identity is not observed; retain resources",
        )?;
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn await_saved_close(path: &Path, binding: &J) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let (_, _, mut saved) = read_snapshot(path)?;
        if !snapshot::prune(&mut saved, binding)? {
            return Ok(());
        }
        ensure(
            Instant::now() < deadline,
            "Herdr close is not persisted; retain cleanup obligation",
        )?;
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn reclaim_directory(root: &Path, binding: &J) -> Result<()> {
    if !root.try_exists()? {
        return Ok(());
    }
    store_directory(root)?;
    for (index, entry) in fs::read_dir(root)?.enumerate() {
        ensure(index < 256, "Herdr recovery inventory exceeds bound")?;
        let path = entry?.path();
        if is_snapshot(&path) {
            reclaim_file(&path, binding)?;
        }
    }
    Ok(())
}

fn is_snapshot(path: &Path) -> bool {
    path.file_name()
        .and_then(|v| v.to_str())
        .is_some_and(|name| name.starts_with("session-") && name.ends_with(".json"))
}

fn read_snapshot(path: &Path) -> Result<(fs::Metadata, Vec<u8>, J)> {
    use std::io::Read;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let metadata = file.metadata()?;
    ensure(
        metadata.is_file()
            && metadata.nlink() == 1
            && metadata.uid() == util::process::current_uid(),
        "Herdr persistence entry is not an owned regular file",
    )?;
    let mut bytes = Vec::new();
    file.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
    ensure(
        bytes.len() as u64 <= MAX_BYTES,
        "Herdr persistence entry exceeds bound",
    )?;
    let value = serde_json::from_slice(&bytes)?;
    Ok((metadata, bytes, value))
}

fn reclaim_file(path: &Path, binding: &J) -> Result<()> {
    let (metadata, bytes, mut value) = read_snapshot(path)?;
    // Historical foreign formats are untouched. A shell may retain only cwd;
    // hints select candidates, while captured identities establish ownership.
    if !may_reference_task(&bytes, binding)? {
        return Ok(());
    }
    if snapshot::prune(&mut value, binding)? {
        replace_snapshot(path, &metadata, &bytes, &serde_json::to_vec(&value)?)?;
    }
    Ok(())
}

fn may_reference_task(bytes: &[u8], binding: &J) -> Result<bool> {
    let hints = [required(binding, "session")?, required(binding, "cwd")?];
    let text = String::from_utf8_lossy(bytes);
    Ok(hints
        .into_iter()
        .any(|hint| text.contains(&J::String(hint).to_string())))
}

fn replace_snapshot(
    path: &Path,
    metadata: &fs::Metadata,
    before: &[u8],
    after: &[u8],
) -> Result<()> {
    let temporary = prepare_replacement(path, metadata, after)?;
    // Exchange cannot recreate a snapshot concurrently pruned by Herdr. It also
    // retains the exact displaced object until its identity/content is checked.
    exchange(path, temporary.path())?;
    if verify_displaced(temporary.path(), metadata, before).is_err() {
        rollback(path, temporary)?;
        return Err(FlowError::guard(
            "Herdr recovery copy changed; cleanup not confirmed",
        ));
    }
    fs::File::open(path.parent().unwrap())?.sync_all()?;
    Ok(())
}

fn prepare_replacement(
    path: &Path,
    metadata: &fs::Metadata,
    after: &[u8],
) -> Result<tempfile::NamedTempFile> {
    use std::io::Write;
    let mut temporary = tempfile::NamedTempFile::new_in(path.parent().unwrap())?;
    temporary.write_all(after)?;
    temporary
        .as_file()
        .set_permissions(metadata.permissions())?;
    temporary
        .as_file()
        .set_times(fs::FileTimes::new().set_modified(metadata.modified()?))?;
    temporary.as_file().sync_all()?;
    Ok(temporary)
}

fn verify_displaced(path: &Path, expected: &fs::Metadata, before: &[u8]) -> Result<()> {
    let displaced = fs::symlink_metadata(path)?;
    ensure(
        (displaced.dev(), displaced.ino()) == (expected.dev(), expected.ino()),
        "Herdr recovery copy identity changed",
    )?;
    ensure(
        fs::read(path)? == before,
        "Herdr recovery copy contents changed",
    )
}

fn rollback(path: &Path, temporary: tempfile::NamedTempFile) -> Result<()> {
    let result =
        verify_installed(path, temporary.as_file()).and_then(|()| exchange(path, temporary.path()));
    if let Err(error) = result {
        let (_, retained) = temporary.keep().map_err(|e| FlowError::io(e.to_string()))?;
        return Err(FlowError::guard(format!(
            "Herdr recovery-copy rollback failed: {error}; inspect {}",
            retained.display()
        )));
    }
    Ok(())
}

fn verify_installed(path: &Path, file: &fs::File) -> Result<()> {
    let expected = file.metadata()?;
    let actual = fs::symlink_metadata(path)?;
    ensure(
        (actual.dev(), actual.ino()) == (expected.dev(), expected.ino()),
        "Herdr recovery copy changed again during rollback",
    )
}

fn exchange(first: &Path, second: &Path) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let first = std::ffi::CString::new(first.as_os_str().as_bytes())
        .map_err(|e| FlowError::io(e.to_string()))?;
    let second = std::ffi::CString::new(second.as_os_str().as_bytes())
        .map_err(|e| FlowError::io(e.to_string()))?;
    // SAFETY: both paths are live NUL-terminated strings; exchange only swaps
    // existing names and never follows a leaf symlink or recreates a missing one.
    let result = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            first.as_ptr(),
            libc::AT_FDCWD,
            second.as_ptr(),
            libc::RENAME_EXCHANGE,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}
