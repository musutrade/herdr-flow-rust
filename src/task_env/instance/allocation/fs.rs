//! Descriptor-anchored allocation. Mount identity is per transfer, not global.
use super::super::{require, RootIdentity};
use crate::{util, FlowError, Result};
use serde::{Deserialize, Serialize};
use std::{
    ffi::{CString, OsStr},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::{
        fd::AsRawFd,
        unix::{
            ffi::OsStrExt,
            fs::{MetadataExt, OpenOptionsExt},
        },
    },
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Directory {
    pub path: PathBuf,
    pub identity: RootIdentity,
    pub mount: u64,
}

impl Directory {
    pub fn capture(path: &Path) -> Result<Self> {
        let fd = util::fs::secure::directory(path, false)?;
        let proof = Self {
            path: path.to_path_buf(),
            identity: RootIdentity::of(&fd.metadata()?),
            mount: mount_id(&fd)?,
        };
        proof.verify()?;
        Ok(proof)
    }
    pub fn verify(&self) -> Result<File> {
        let fd = util::fs::secure::directory(&self.path, false)?;
        require(
            RootIdentity::of(&fd.metadata()?) == self.identity && mount_id(&fd)? == self.mount,
            "allocation directory identity or mount changed",
        )?;
        Ok(fd)
    }
}

pub fn mount_id(fd: &File) -> Result<u64> {
    let mut stat: libc::statx = unsafe { std::mem::zeroed() };
    // SAFETY: fd remains open; empty C string and writable statx are valid.
    let rc = unsafe {
        libc::statx(
            fd.as_raw_fd(),
            c"".as_ptr(),
            libc::AT_EMPTY_PATH | libc::AT_SYMLINK_NOFOLLOW,
            libc::STATX_MNT_ID,
            &mut stat,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    require(
        stat.stx_mask & libc::STATX_MNT_ID != 0,
        "allocation requires statx mount identity",
    )?;
    Ok(stat.stx_mnt_id)
}

pub fn name(name: &OsStr) -> Result<CString> {
    require(
        Path::new(name).components().count() == 1
            && matches!(
                Path::new(name).components().next(),
                Some(std::path::Component::Normal(_))
            ),
        "allocation name is not one component",
    )?;
    CString::new(name.as_bytes()).map_err(|_| FlowError::guard("allocation name contains NUL"))
}

pub fn anchored(fd: &File, name: &OsStr) -> Result<PathBuf> {
    self::name(name)?;
    Ok(PathBuf::from(format!("/proc/self/fd/{}", fd.as_raw_fd())).join(name))
}

pub fn mkdir(parent: &File, slot: &OsStr) -> Result<()> {
    let slot = name(slot)?;
    // SAFETY: live parent descriptor, single NUL-terminated component.
    if unsafe { libc::mkdirat(parent.as_raw_fd(), slot.as_ptr(), 0o700) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    #[cfg(test)]
    super::tests::checkpoint(&format!("mkdir:{}", slot.to_string_lossy()));
    Ok(())
}

pub fn rename(from: &File, slot: &OsStr, to: &File, target: &OsStr) -> Result<()> {
    require(
        mount_id(from)? == mount_id(to)?,
        "allocation transfer crosses mounts",
    )?;
    let slot = name(slot)?;
    let target = name(target)?;
    // SAFETY: both descriptors remain open; names are single components. Never
    // overwrite a target or fall back to copy/delete or a second mkdir.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            from.as_raw_fd(),
            slot.as_ptr(),
            to.as_raw_fd(),
            target.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

pub fn absent(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(e) => Err(e.into()),
        Ok(_) => Ok(false),
    }
}

pub fn sync_pair(first: &File, second: &File) -> Result<()> {
    first.sync_all()?;
    second.sync_all()?;
    Ok(())
}

pub fn open_child(parent: &File, slot: &OsStr) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY | libc::O_CLOEXEC)
        .open(anchored(parent, slot)?)?;
    let m = file.metadata()?;
    require(
        m.uid() == util::process::current_uid() && m.mode() & 0o077 == 0,
        "allocation child is not private",
    )?;
    require(
        mount_id(&file)? == mount_id(parent)?,
        "allocation child is a different mount",
    )?;
    Ok(file)
}

/// Only for a slot already reserved by a durable parent authority. Existing
/// objects here are internal allocation data, never user target paths.
pub fn reserved_dir(parent: &File, slot: &OsStr) -> Result<File> {
    if absent(&anchored(parent, slot)?)? {
        mkdir(parent, slot)?;
    }
    let child = open_child(parent, slot)?;
    sync_pair(&child, parent)?;
    Ok(child)
}

pub fn entries(directory: &File, allowed: &[&str]) -> Result<()> {
    let path = PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()));
    for e in fs::read_dir(path)? {
        let e = e?;
        require(
            allowed.iter().any(|n| e.file_name() == *n),
            "unexpected object in reserved allocation slot",
        )?;
    }
    Ok(())
}

/// Complete a possibly interrupted initial write of these exact bytes. No
/// ordinary document updater calls this; its parent slot reservation is required.
pub fn initial_file(directory: &File, name: &str, bytes: &[u8]) -> Result<()> {
    let path = anchored(directory, OsStr::new(name))?;
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    validate_initial_file(&file, bytes.len())?;
    complete_initial_file(&mut file, directory, name, bytes)
}
fn validate_initial_file(file: &File, length: usize) -> Result<()> {
    let m = file.metadata()?;
    require(
        m.is_file()
            && m.uid() == util::process::current_uid()
            && m.mode() & 0o077 == 0
            && m.len() <= length as u64,
        "invalid reserved bootstrap file",
    )
}
fn complete_initial_file(
    file: &mut File,
    directory: &File,
    name: &str,
    bytes: &[u8],
) -> Result<()> {
    let mut old = Vec::new();
    file.read_to_end(&mut old)?;
    require(bytes.starts_with(&old), "reserved bootstrap bytes conflict")?;
    let middle = old.len() + (bytes.len() - old.len()) / 2;
    file.write_all(&bytes[old.len()..middle])?;
    #[cfg(test)]
    initial_checkpoint(directory, name);
    let _ = name;
    file.write_all(&bytes[middle..])?;
    file.sync_all()?;
    directory.sync_all()?;
    Ok(())
}
#[cfg(test)]
fn initial_checkpoint(directory: &File, name: &str) {
    super::tests::checkpoint(&format!("initial:{name}"));
    let path = std::fs::read_link(format!("/proc/self/fd/{}", directory.as_raw_fd())).unwrap();
    super::tests::checkpoint(&format!(
        "initial:{}:{name}",
        path.file_name().unwrap().to_string_lossy()
    ));
}

pub fn marker(directory: &File, document: bool, namespace: bool) -> Result<()> {
    if document {
        initial_file(directory, ".flow-document-v2", util::protected::FORMAT)?;
    }
    if namespace {
        initial_file(directory, ".flow-namespace-v2", util::protected::FORMAT)?;
    }
    Ok(())
}

pub fn write_document(directory: &File, bytes: &[u8]) -> Result<()> {
    let mut file = open_scratch(directory)?;
    write_scratch(&mut file, bytes)?;
    replace_payload(directory)
}
fn open_scratch(directory: &File) -> Result<File> {
    let path = anchored(directory, OsStr::new(".allocation-next"))?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    private_regular(&file, "invalid allocation checkpoint scratch")?;
    Ok(file)
}
fn write_scratch(file: &mut File, bytes: &[u8]) -> Result<()> {
    file.set_len(0)?;
    file.write_all(&bytes[..bytes.len() / 2])?;
    #[cfg(test)]
    super::tests::checkpoint("checkpoint-half-written");
    file.write_all(&bytes[bytes.len() / 2..])?;
    file.sync_all()?;
    #[cfg(test)]
    super::tests::checkpoint("checkpoint-scratch-synced");
    Ok(())
}
fn replace_payload(directory: &File) -> Result<()> {
    let scratch = anchored(directory, OsStr::new(".allocation-next"))?;
    let target = anchored(directory, OsStr::new("authority.json"))?;
    require(
        fs::symlink_metadata(&target)?.is_file(),
        "allocation payload replaced",
    )?;
    fs::rename(&scratch, &target)?;
    #[cfg(test)]
    super::tests::checkpoint("checkpoint-renamed");
    directory.sync_all()?;
    Ok(())
}
fn private_regular(file: &File, message: &str) -> Result<()> {
    let m = file.metadata()?;
    require(
        m.is_file() && m.uid() == util::process::current_uid() && m.mode() & 0o077 == 0,
        message,
    )
}

pub fn remove_empty(directory: &File, slot: &OsStr, identity: RootIdentity) -> Result<()> {
    let child = open_child(directory, slot)?;
    require(
        RootIdentity::of(&child.metadata()?) == identity,
        "allocation slot identity changed",
    )?;
    entries(&child, &[])?;
    let slot = name(slot)?;
    // SAFETY: pinned directory and single component; rmdir never traverses data.
    if unsafe { libc::unlinkat(directory.as_raw_fd(), slot.as_ptr(), libc::AT_REMOVEDIR) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    directory.sync_all()?;
    Ok(())
}

pub fn lock_file(parent: &File, name: &str) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(anchored(parent, OsStr::new(name))?)?;
    let m = file.metadata()?;
    require(
        m.is_file() && m.uid() == util::process::current_uid() && m.mode() & 0o077 == 0,
        "allocation lock is not private",
    )?;
    Ok(file)
}

pub fn read_initial(parent: &File, name: &str) -> Result<Option<Vec<u8>>> {
    let path = anchored(parent, OsStr::new(name))?;
    if absent(&path)? {
        return Ok(None);
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)?;
    private_regular(&file, "invalid bootstrap document")?;
    Ok(Some(read_bounded(file)?))
}
fn read_bounded(file: File) -> Result<Vec<u8>> {
    const LIMIT: u64 = 4 * 1024 * 1024;
    require(
        file.metadata()?.len() <= LIMIT,
        "bootstrap document too large",
    )?;
    let mut bytes = Vec::new();
    file.take(LIMIT + 1).read_to_end(&mut bytes)?;
    require(bytes.len() as u64 <= LIMIT, "bootstrap document too large")?;
    Ok(bytes)
}
