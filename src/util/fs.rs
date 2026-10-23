//! Atomic durable writes, path resolution, ownership checks and flock helpers.

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

use serde_json::Value;
use tempfile::Builder;

use crate::error::{FlowError, Result};

mod secure;

pub fn read_bytes(path: &Path) -> Result<Vec<u8>> {
    fs::read(path)
        .map_err(|error| FlowError::io(format!("cannot read {}: {error}", path.display())))
}

pub fn read_text(path: &Path) -> Result<String> {
    let bytes = read_bytes(path)?;
    String::from_utf8(bytes)
        .map_err(|error| FlowError::new(crate::error::ErrorKind::Json, error.to_string()))
}

pub fn read_json(path: &Path) -> Result<Value> {
    let text = read_text(path)?;
    serde_json::from_str(&text).map_err(|error| {
        FlowError::new(
            crate::error::ErrorKind::Json,
            format!("{}: {error}", path.display()),
        )
    })
}

pub fn read_json_if_exists(path: &Path) -> Result<Option<Value>> {
    match fs::metadata(path) {
        Ok(_) => Ok(Some(read_json(path)?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// Python `Path.resolve(strict=False)`: absolute, symlink-aware for the
/// existing prefix, lexically normalized for the rest.
pub fn resolve_path(path: &Path) -> PathBuf {
    let absolute = absolutize(path);
    let normalized = normalize_components(&absolute);
    let (existing, missing) = split_missing(normalized);
    append_missing(canonicalize_or_self(&existing), &missing)
}

fn absolutize(path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("/"))
        .join(path)
}

fn normalize_components(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::from("/");
    for component in path.components() {
        push_component(&mut normalized, component);
    }
    normalized
}

fn push_component(normalized: &mut PathBuf, component: Component<'_>) {
    match component {
        Component::RootDir | Component::Prefix(_) => {}
        Component::CurDir => {}
        Component::ParentDir => pop_resolved(normalized),
        Component::Normal(name) => normalized.push(name),
    }
}

fn pop_resolved(path: &mut PathBuf) {
    // Python `Path.resolve` follows symlinks before applying `..`: for
    // `link/../child` with `link -> elsewhere/inner`, the result is
    // `elsewhere/child`, not `child` next to the lexical link.
    let current = path.canonicalize().unwrap_or_else(|_| path.clone());
    *path = current
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("/"));
}

fn split_missing(mut cursor: PathBuf) -> (PathBuf, Vec<std::ffi::OsString>) {
    let mut missing: Vec<std::ffi::OsString> = Vec::new();
    while !cursor.exists() {
        let Some(name) = cursor.file_name() else {
            break;
        };
        missing.push(name.to_os_string());
        if !cursor.pop() {
            break;
        }
    }
    (cursor, missing)
}

fn canonicalize_or_self(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn append_missing(mut result: PathBuf, missing: &[std::ffi::OsString]) -> PathBuf {
    for name in missing.iter().rev() {
        result.push(name);
    }
    result
}

pub fn absolute(path: &Path) -> bool {
    path.is_absolute()
}

/// True when `path` is inside `base` (both should be resolved by the caller
/// where the Python implementation resolved them).
pub fn is_relative_to(path: &Path, base: &Path) -> bool {
    path.starts_with(base)
}

/// True when either resolved path contains the other.
pub fn overlap(left: &Path, right: &Path) -> bool {
    let left = resolve_path(left);
    let right = resolve_path(right);
    left.starts_with(&right) || right.starts_with(&left)
}

pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

pub fn xdg_state_home() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".local/state"))
}

pub fn codex_home() -> PathBuf {
    std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".codex"))
}

/// Atomic replacement in an anchored directory, without following symlinks.
/// The file is private, fsynced before rename, and its directory is fsynced after.
///
/// The caller must hold its state lock when serializing read/modify/write
/// operations; this helper does not acquire one or prevent competing trusted
/// writers. Missing parent directories may be created. Symlink/owner/mode
/// checks protect the anchored path under the trusted-owner model.
/// An error after rename (for example directory fsync failure) can mean the new
/// content is already visible. Inspect/reconcile durable state before retrying
/// a larger operation; an error does not promise rollback. Rewriting identical
/// bytes is allowed, but replaces inode/metadata and is not a no-op.
pub fn atomic_write_bytes(path: &Path, data: &[u8]) -> Result<()> {
    let name = path
        .file_name()
        .ok_or_else(|| FlowError::io("write target has no filename"))?;
    let parent = path
        .parent()
        .ok_or_else(|| FlowError::io("write target has no parent"))?;
    let directory = secure::directory(parent, true)?;
    let anchor = secure::anchor(&directory);
    let target = anchor.join(name);
    reject_leaf_symlink(&target)?;
    let temporary = create_temporary(&anchor)?;
    write_temporary(temporary, data, &target)?;
    directory.sync_all()?;
    Ok(())
}

fn reject_leaf_symlink(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Err(FlowError::io("write target cannot be a symlink"))
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn create_temporary(parent: &Path) -> Result<tempfile::NamedTempFile> {
    Builder::new()
        .prefix(".write-")
        .tempfile_in(parent)
        .map_err(|error| FlowError::io(format!("cannot create temporary file: {error}")))
}

fn write_temporary(mut temporary: tempfile::NamedTempFile, data: &[u8], path: &Path) -> Result<()> {
    set_temporary_mode(&mut temporary)?;
    write_temporary_data(&mut temporary, data)?;
    sync_temporary(&mut temporary)?;
    persist_temporary(temporary, path)
}

fn set_temporary_mode(temporary: &mut tempfile::NamedTempFile) -> Result<()> {
    temporary
        .as_file_mut()
        .set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok(())
}

fn write_temporary_data(temporary: &mut tempfile::NamedTempFile, data: &[u8]) -> Result<()> {
    temporary
        .as_file_mut()
        .write_all(data)
        .map_err(|error| FlowError::io(format!("cannot write temporary file: {error}")))
}

fn sync_temporary(temporary: &mut tempfile::NamedTempFile) -> Result<()> {
    temporary
        .as_file_mut()
        .sync_all()
        .map_err(|error| FlowError::io(format!("cannot fsync temporary file: {error}")))
}

fn persist_temporary(temporary: tempfile::NamedTempFile, path: &Path) -> Result<()> {
    temporary.persist(path).map_err(|error| {
        FlowError::io(format!(
            "cannot replace {}: {}",
            path.display(),
            error.error
        ))
    })?;
    Ok(())
}

pub fn atomic_write_json(path: &Path, value: &Value) -> Result<()> {
    let mut text = serde_json::to_string_pretty(value)
        .map_err(|error| FlowError::new(crate::error::ErrorKind::Json, error.to_string()))?;
    text.push('\n');
    atomic_write_bytes(path, text.as_bytes())
}

pub fn atomic_write_json_compact(path: &Path, value: &Value) -> Result<()> {
    let mut text = serde_json::to_string(value)
        .map_err(|error| FlowError::new(crate::error::ErrorKind::Json, error.to_string()))?;
    text.push('\n');
    atomic_write_bytes(path, text.as_bytes())
}

pub fn write_new_file(path: &Path, value: &Value) -> Result<()> {
    // Python `open('x')` reservation semantics: create only if absent, and
    // never expose a partially written document to a concurrent reader.
    let (directory, path) = new_file_target(path)?;
    let parent = secure::anchor(&directory);
    let mut text = serde_json::to_string_pretty(value)
        .map_err(|error| FlowError::new(crate::error::ErrorKind::Json, error.to_string()))?;
    text.push('\n');
    let mut temporary = Builder::new()
        .prefix(".new-")
        .tempfile_in(&parent)
        .map_err(|error| FlowError::io(format!("cannot create temporary file: {error}")))?;
    temporary
        .as_file_mut()
        .set_permissions(fs::Permissions::from_mode(0o600))?;
    temporary
        .as_file_mut()
        .write_all(text.as_bytes())
        .map_err(|error| FlowError::io(format!("cannot write temporary file: {error}")))?;
    temporary
        .as_file_mut()
        .sync_all()
        .map_err(|error| FlowError::io(format!("cannot fsync temporary file: {error}")))?;
    temporary
        .persist_noclobber(&path)
        .map_err(|error| new_file_error(error, &path))?;
    directory.sync_all()?;
    Ok(())
}

fn new_file_error(error: tempfile::PersistError, path: &Path) -> FlowError {
    if error.error.kind() == std::io::ErrorKind::AlreadyExists {
        FlowError::io(format!("{} already exists", path.display()))
    } else {
        FlowError::io(format!("cannot create {}: {}", path.display(), error.error))
    }
}

fn new_file_target(path: &Path) -> Result<(File, PathBuf)> {
    let parent = path
        .parent()
        .ok_or_else(|| FlowError::io("write target has no parent directory"))?;
    let directory = secure::directory(parent, true)?;
    let name = path
        .file_name()
        .ok_or_else(|| FlowError::io("new file has no filename"))?;
    let parent = secure::anchor(&directory);
    let path = parent.join(name);
    Ok((directory, path))
}

pub fn fsync_dir(path: &Path) -> Result<()> {
    let directory = File::open(path)?;
    directory.sync_all()?;
    Ok(())
}

pub enum LockProbe {
    Acquired(FileLock),
    Busy,
    Missing,
}

pub struct FileLock {
    file: File,
}

impl FileLock {
    /// Open (or create) `path` and take a `flock`. With `blocking=false`,
    /// returns `Ok(None)` when another process owns the lock.
    pub fn acquire(path: &Path, blocking: bool) -> Result<Option<Self>> {
        let parent = path
            .parent()
            .ok_or_else(|| FlowError::io("lock has no parent"))?;
        let name = path
            .file_name()
            .ok_or_else(|| FlowError::io("lock has no filename"))?;
        let directory = secure::directory(parent, false)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .truncate(false)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
            .open(secure::anchor(&directory).join(name))?;
        validate_state_file(&file)?;
        Self::from_file(file, blocking)
    }

    fn from_file(file: File, blocking: bool) -> Result<Option<Self>> {
        let operation = libc::LOCK_EX | if blocking { 0 } else { libc::LOCK_NB };
        // SAFETY: file owns a live descriptor throughout flock; the syscall
        // takes scalar flags and does not take descriptor ownership.
        let result = unsafe { libc::flock(file.as_raw_fd(), operation) };
        if result == 0 {
            return Ok(Some(Self { file }));
        }
        let error = std::io::Error::last_os_error();
        let code = error.raw_os_error();
        if !blocking && matches!(code, Some(libc::EWOULDBLOCK)) {
            return Ok(None);
        }
        Err(FlowError::io(format!("cannot lock file: {error}")))
    }

    pub fn as_raw_fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }
}

/// Probe an existing lock without creating, changing its mode or removing it.
/// Missing is explicit and is never reported as free. Symlinks, non-regular
/// files and open/validation failures are returned as errors.
pub fn probe_existing_lock(path: &Path) -> Result<LockProbe> {
    let parent = path
        .parent()
        .ok_or_else(|| FlowError::io("lock has no parent"))?;
    let name = path
        .file_name()
        .ok_or_else(|| FlowError::io("lock has no filename"))?;
    let directory = secure::directory(parent, false)?;
    let file = match OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(secure::anchor(&directory).join(name))
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(LockProbe::Missing)
        }
        Err(error) => {
            return Err(FlowError::io(format!(
                "cannot probe existing lock: {error}"
            )))
        }
    };
    validate_state_file(&file)?;
    match FileLock::from_file(file, false)? {
        Some(lock) => Ok(LockProbe::Acquired(lock)),
        None => Ok(LockProbe::Busy),
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        // SAFETY: self.file is still live during Drop. Unlock before File's own
        // destructor closes the descriptor; flock does not consume it.
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

/// A nonblocking lock probe. Returns true when the lock is free.
pub fn lock_is_free(path: &Path) -> bool {
    matches!(FileLock::acquire(path, false), Ok(Some(_)))
}

/// Try to take an exclusive flock without blocking. Returns true on success
/// and false when another owner holds the lock.
///
/// Borrows `fd`; the caller keeps it open for the call and retains ownership.
/// Success leaves the lock held on its open file description until explicit
/// unlock or final close. Duplicated descriptors share that lock. This helper
/// neither returns an RAII guard nor prevents concurrent changes through aliases.
pub fn flock_try_exclusive(fd: RawFd) -> Result<bool> {
    // SAFETY: caller passes a live descriptor; flock takes only scalars.
    let result = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
    if result == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    if matches!(error.raw_os_error(), Some(libc::EWOULDBLOCK)) {
        return Ok(false);
    }
    Err(FlowError::io(format!("cannot lock file: {error}")))
}

/// Take an exclusive blocking flock on an already-open file descriptor.
///
/// Borrows `fd` without closing it. Success leaves the lock held; the caller
/// must release it by unlocking or closing the final shared description.
/// There is no timeout; do not hold conflicting application locks while waiting.
/// Errors do not transfer ownership and do not imply any state transaction ran.
pub fn flock_exclusive(fd: RawFd) -> Result<()> {
    // SAFETY: caller must ensure fd is valid and live during this call.
    let result = unsafe { libc::flock(fd, libc::LOCK_EX) };
    if result == 0 {
        Ok(())
    } else {
        let error = std::io::Error::last_os_error();
        Err(FlowError::io(format!("cannot lock file: {error}")))
    }
}

#[derive(Clone, Copy, Debug)]
pub struct FileIdentity {
    pub dev: u64,
    pub ino: u64,
    pub uid: u32,
    pub mode: u32,
    pub size: u64,
}

pub fn metadata_identity(path: &Path) -> Result<FileIdentity> {
    let metadata = fs::symlink_metadata(path)?;
    Ok(FileIdentity {
        dev: metadata.dev(),
        ino: metadata.ino(),
        uid: metadata.uid(),
        mode: metadata.mode(),
        size: metadata.size(),
    })
}

pub fn is_socket(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|metadata| metadata.file_type().is_socket())
        .unwrap_or(false)
}

pub fn is_owned_regular_file(path: &Path) -> bool {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return false;
    };
    // SAFETY: geteuid takes no pointers and has no caller preconditions.
    metadata.file_type().is_file() && metadata.uid() == unsafe { libc::geteuid() }
}

/// Create missing directories as 0700. Existing 0700/0755 directories are
/// accepted only when owned by this user and not group/world writable.
pub fn ensure_private_dir(path: &Path, create: bool) -> Result<()> {
    let directory = secure::directory(path, create)?;
    let metadata = directory.metadata()?;
    validate_private_dir_type(&metadata)?;
    validate_private_dir_owner(&metadata)?;
    if metadata.mode() & 0o022 != 0 {
        return Err(FlowError::guard(
            "state directory must not be group/world writable",
        ));
    }
    Ok(())
}

pub fn state_dir(path: &Path, create: bool) -> Result<PathBuf> {
    ensure_private_dir(path, create)?;
    Ok(resolve_path(path))
}

pub fn read_state_json(root: &Path, name: &str) -> Result<Value> {
    if Path::new(name).file_name() != Some(std::ffi::OsStr::new(name)) {
        return Err(FlowError::io("state filename must be a basename"));
    }
    ensure_private_dir(root, false)?;
    let directory = secure::directory(root, false)?;
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(secure::anchor(&directory).join(name))?;
    validate_state_file(&file)?;
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    Ok(serde_json::from_str(&text)?)
}

/// Read one state document without following links or exceeding `limit` bytes.
pub fn read_state_json_bounded(root: &Path, name: &str, limit: usize) -> Result<Value> {
    read_state_json_bounded_if_exists(root, name, limit)?
        .ok_or_else(|| FlowError::io("state document is missing"))
}

/// Bounded state read that returns `None` only when the owned regular file is
/// absent. Symlinks, non-regular files, oversized documents and parse errors
/// fail closed.
pub fn read_state_json_bounded_if_exists(
    root: &Path,
    name: &str,
    limit: usize,
) -> Result<Option<Value>> {
    if Path::new(name).file_name() != Some(std::ffi::OsStr::new(name)) {
        return Err(FlowError::io("state filename must be a basename"));
    }
    ensure_private_dir(root, false)?;
    let directory = secure::directory(root, false)?;
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(secure::anchor(&directory).join(name))
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    validate_state_file(&file)?;
    let mut bytes = Vec::new();
    Read::by_ref(&file)
        .take((limit + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(FlowError::guard(
            "state document exceeds bounded read limit",
        ));
    }
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|error| FlowError::new(crate::error::ErrorKind::Json, error.to_string()))
}

fn validate_state_file(file: &File) -> Result<()> {
    let metadata = file.metadata()?;
    // SAFETY: geteuid takes no pointers and has no caller preconditions.
    if !metadata.is_file() || metadata.uid() != unsafe { libc::geteuid() } {
        return Err(FlowError::guard(
            "state/lock file must be an owned regular file",
        ));
    }
    if metadata.mode() & 0o022 != 0 {
        return Err(FlowError::guard(
            "state/lock file must not be group/world writable",
        ));
    }
    Ok(())
}

fn validate_private_dir_type(metadata: &fs::Metadata) -> Result<()> {
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(FlowError::guard(
            "state directory must be an owned real directory",
        ));
    }
    Ok(())
}

fn validate_private_dir_owner(metadata: &fs::Metadata) -> Result<()> {
    // SAFETY: geteuid takes no pointers and has no caller preconditions.
    if metadata.uid() != unsafe { libc::geteuid() } {
        return Err(FlowError::guard(
            "state directory must be owned by this user",
        ));
    }
    Ok(())
}

pub fn remove_file_if_exists(path: &Path) -> Result<()> {
    fs::remove_file(path).or_else(ignore_not_found)?;
    Ok(())
}

/// Remove one private-state entry through a pinned, no-symlink directory.
/// Missing sidecar directories are normal during interrupted cleanup.
pub fn remove_state_file(root: &Path, name: &str) -> Result<()> {
    if Path::new(name).file_name() != Some(std::ffi::OsStr::new(name)) {
        return Err(FlowError::io("state filename must be a basename"));
    }
    if !root.try_exists()? {
        return Ok(());
    }
    let directory = secure::directory(root, false)?;
    remove_file_if_exists(&secure::anchor(&directory).join(name))?;
    directory.sync_all()?;
    Ok(())
}

fn ignore_not_found(error: std::io::Error) -> std::io::Result<()> {
    if error.kind() == std::io::ErrorKind::NotFound {
        return Ok(());
    }
    Err(error)
}

pub fn read_file_limited(path: &Path, limit: usize) -> Result<Vec<u8>> {
    let mut file = File::open(path)?;
    let mut buffer = Vec::new();
    Read::by_ref(&mut file)
        .take(limit as u64)
        .read_to_end(&mut buffer)?;
    Ok(buffer)
}

/// Recursively list regular files whose basename ends with `suffix`.
pub fn files_with_suffix(root: &Path, suffix: &str) -> Vec<PathBuf> {
    let mut result = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    let mut visited = HashSet::new();
    while let Some(directory) = stack.pop() {
        let Ok(canonical) = directory.canonicalize() else {
            continue;
        };
        if !visited.insert(canonical) {
            continue;
        }
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(metadata) = fs::metadata(&path) else {
                continue;
            };
            if metadata.is_dir() {
                stack.push(path);
            } else if metadata.is_file()
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .map(|name| name.ends_with(suffix))
                    .unwrap_or(false)
            {
                result.push(path);
            }
        }
    }
    result
}

pub fn pid_is_alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn resolve_path_follows_symlinks_and_keeps_missing_suffix() {
        let directory = crate::util::fs::test_tempdir();
        let target = directory.path().join("target");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("file"), b"x").unwrap();
        let link = directory.path().join("link");
        symlink(&target, &link).unwrap();
        assert_eq!(resolve_path(&link.join("file")), target.join("file"));
        let missing = link.join("missing").join("..").join("child");
        assert_eq!(resolve_path(&missing), target.join("child"));
    }

    #[test]
    fn resolve_path_applies_parent_after_symlink_resolution() {
        let directory = crate::util::fs::test_tempdir();
        let base = directory.path();
        let target = base.join("elsewhere").join("inner");
        fs::create_dir_all(&target).unwrap();
        let link = base.join("link");
        symlink(&target, &link).unwrap();
        assert_eq!(
            resolve_path(&link.join("..").join("child")),
            base.join("elsewhere").join("child")
        );
    }

    #[test]
    fn atomic_writes_are_private_and_reject_symlink_parents() {
        let directory = crate::util::fs::test_tempdir();
        let parent = directory.path().join("nested");
        let path = parent.join("state.json");
        atomic_write_bytes(&path, b"payload").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"payload");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(&parent).unwrap().permissions().mode() & 0o777,
            0o700
        );

        let real = directory.path().join("real");
        fs::create_dir(&real).unwrap();
        let link = directory.path().join("link");
        symlink(&real, &link).unwrap();
        assert!(atomic_write_bytes(&link.join("state.json"), b"x").is_err());
    }

    #[test]
    fn lock_probe_reports_free_and_held_states() {
        let directory = crate::util::fs::test_tempdir();
        let path = directory.path().join("state.lock");
        assert!(lock_is_free(&path));
        let lock = FileLock::acquire(&path, false).unwrap().unwrap();
        assert!(!lock_is_free(&path));
        drop(lock);
        assert!(lock_is_free(&path));
    }

    #[test]
    fn existing_lock_probe_never_creates_or_changes_the_lock() {
        let directory = crate::util::fs::test_tempdir();
        let path = directory.path().join("guard.lock");
        assert!(matches!(probe_existing_lock(&path), Ok(LockProbe::Missing)));
        assert!(!path.exists());

        let lock = FileLock::acquire(&path, false).unwrap().unwrap();
        let bytes = fs::read(&path).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        drop(lock);
        assert!(matches!(
            probe_existing_lock(&path),
            Ok(LockProbe::Acquired(_))
        ));
        assert_eq!(fs::read(&path).unwrap(), bytes);
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode(), mode);

        let held = FileLock::acquire(&path, false).unwrap().unwrap();
        assert!(matches!(probe_existing_lock(&path), Ok(LockProbe::Busy)));
        drop(held);
    }

    #[test]
    fn existing_lock_probe_rejects_symlink_and_nonregular_paths() {
        let directory = crate::util::fs::test_tempdir();
        let target = directory.path().join("target");
        fs::write(&target, b"not a lock").unwrap();
        let link = directory.path().join("link.lock");
        symlink(&target, &link).unwrap();
        assert!(probe_existing_lock(&link).is_err());
        assert!(probe_existing_lock(directory.path()).is_err());
        let writable = directory.path().join("writable.lock");
        fs::write(&writable, b"x").unwrap();
        fs::set_permissions(&writable, fs::Permissions::from_mode(0o666)).unwrap();
        assert!(probe_existing_lock(&writable).is_err());
        // SAFETY: geteuid takes no pointer arguments or caller preconditions.
        if unsafe { libc::geteuid() } != 0 {
            let unreadable = directory.path().join("unreadable.lock");
            fs::write(&unreadable, b"x").unwrap();
            fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o000)).unwrap();
            assert!(probe_existing_lock(&unreadable).is_err());
        }
    }

    #[test]
    fn bounded_state_json_reads_fail_closed_without_creating_files() {
        let directory = crate::util::fs::test_tempdir();
        let valid = directory.path().join("valid.json");
        fs::write(&valid, br#"{"value":1}"#).unwrap();
        fs::set_permissions(&valid, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            read_state_json_bounded(directory.path(), "valid.json", 64).unwrap()["value"],
            1
        );
        assert!(
            read_state_json_bounded_if_exists(directory.path(), "missing.json", 64)
                .unwrap()
                .is_none()
        );
        assert!(!directory.path().join("missing.json").exists());
        fs::write(&valid, br#"{"value":12345678901234567890}"#).unwrap();
        fs::set_permissions(&valid, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(read_state_json_bounded(directory.path(), "valid.json", 8).is_err());
        let link = directory.path().join("link.json");
        symlink(&valid, &link).unwrap();
        assert!(read_state_json_bounded(directory.path(), "link.json", 64).is_err());
        let broken = directory.path().join("broken.json");
        fs::write(&broken, b"{").unwrap();
        fs::set_permissions(&broken, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(read_state_json_bounded(directory.path(), "broken.json", 64).is_err());
    }

    #[test]
    fn private_directory_checks_and_removal() {
        let directory = crate::util::fs::test_tempdir();
        let path = directory.path().join("private");
        ensure_private_dir(&path, true).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o700
        );
        ensure_private_dir(&path, false).unwrap();
        let missing = directory.path().join("missing");
        assert!(ensure_private_dir(&missing, false).is_err());

        let link = directory.path().join("link");
        symlink(directory.path(), &link).unwrap();
        assert!(ensure_private_dir(&link, false).is_err());

        let file = directory.path().join("file");
        fs::write(&file, b"x").unwrap();
        remove_file_if_exists(&file).unwrap();
        remove_file_if_exists(&file).unwrap();
    }
}

#[cfg(test)]
mod audit_write_tests {
    use super::*;

    #[test]
    fn write_new_file_never_exposes_partial_content() {
        let directory = crate::util::fs::test_tempdir();
        let path = directory.path().join("reservation.json");
        write_new_file(&path, &serde_json::json!({"value": 1})).unwrap();
        assert!(write_new_file(&path, &serde_json::json!({"value": 2})).is_err());
        let value = read_json(&path).unwrap();
        assert_eq!(value["value"], 1);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

#[cfg(test)]
pub(crate) fn test_tempdir() -> tempfile::TempDir {
    Builder::new()
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir()
        .unwrap()
}
