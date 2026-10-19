//! Directory descriptors bind operations to the inspected directory. Components
//! are opened without following symlinks, including during recursive creation.
use std::ffi::{CString, OsStr};
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

use crate::{FlowError, Result};

pub(super) fn directory(path: &Path, create: bool) -> Result<File> {
    let path = std::env::current_dir()?.join(path);
    let mut directory = File::open("/")?;
    for component in path.components() {
        let name = match component {
            Component::Normal(name) => name,
            Component::ParentDir => OsStr::new(".."),
            _ => continue,
        };
        trusted_ancestor(&directory)?;
        directory = descend(&directory, name, create)?;
    }
    trusted_ancestor(&directory)?;
    Ok(directory)
}

fn trusted_ancestor(directory: &File) -> Result<()> {
    let metadata = directory.metadata()?;
    let uid = unsafe { libc::geteuid() };
    if metadata.uid() != uid && metadata.uid() != 0 {
        return Err(FlowError::guard(
            "directory ancestor has an untrusted owner",
        ));
    }
    let root_sticky = metadata.uid() == 0 && metadata.mode() & libc::S_ISVTX != 0;
    if metadata.mode() & 0o022 != 0 && !root_sticky {
        return Err(FlowError::guard(
            "directory must not be group/world writable",
        ));
    }
    Ok(())
}

fn descend(parent: &File, name: &OsStr, create: bool) -> Result<File> {
    let name =
        CString::new(name.as_bytes()).map_err(|_| FlowError::io("directory contains NUL"))?;
    match open_at(parent, &name) {
        Ok(directory) => Ok(directory),
        Err(error) if create && error.kind() == io::ErrorKind::NotFound => {
            mkdir_at(parent, &name)?;
            Ok(open_at(parent, &name)?)
        }
        Err(error) => Err(FlowError::io(format!(
            "cannot open real directory: {error}"
        ))),
    }
}

fn open_at(parent: &File, name: &CString) -> io::Result<File> {
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn mkdir_at(parent: &File, name: &CString) -> io::Result<()> {
    if unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) } == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.kind() == io::ErrorKind::AlreadyExists {
        return Ok(());
    }
    Err(error)
}

pub(super) fn anchor(directory: &File) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()))
}
