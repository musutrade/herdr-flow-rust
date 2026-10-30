//! Generic filesystem checks only; no legacy markers or cleanup registry.
use super::{Error, Result};
use std::fs::{self, File};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::{ffi::OsStrExt, fs::MetadataExt};
use std::path::{Component, Path, PathBuf};
use std::{ffi::CString, io};

pub(super) struct Directory {
    original: PathBuf,
    pub path: PathBuf,
    pub(super) forbidden: Vec<PathBuf>,
    handle: File,
}

pub(super) fn normalize(path: &Path) -> Result<PathBuf> {
    let absolute = std::env::current_dir()?.join(path);
    let mut result = PathBuf::from("/");
    for component in absolute.components() {
        match component {
            Component::Normal(name) => {
                result.push(name);
                resolve_existing(&mut result)?;
            }
            Component::ParentDir => {
                result.pop();
            }
            _ => (),
        }
    }
    Ok(result)
}

fn resolve_existing(path: &mut PathBuf) -> Result<()> {
    match fs::symlink_metadata(&path) {
        Ok(_) => *path = fs::canonicalize(&path)?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => (),
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

fn check_overlap(path: &Path, forbidden: &[PathBuf]) -> Result<()> {
    for p in forbidden {
        let p = normalize(p)?;
        if path.starts_with(&p) || p.starts_with(path) {
            return Err(Error::Invalid(
                "data/backup root overlaps an explicit forbidden directory".into(),
            ));
        }
    }
    Ok(())
}

impl Directory {
    pub fn open(path: &Path, forbidden: &[PathBuf], create: bool) -> Result<Self> {
        let cwd = std::env::current_dir()?;
        let original = cwd.join(path);
        let forbidden: Vec<_> = forbidden.iter().map(|p| cwd.join(p)).collect();
        let canonical = normalize(&original)?;
        check_overlap(&canonical, &forbidden)?;
        let handle = walk(&canonical, create)?;
        private(&handle.metadata()?, true)?;
        local(&handle)?;
        let dir = Self {
            original,
            path: canonical,
            forbidden,
            handle,
        };
        dir.verify()?;
        Ok(dir)
    }

    pub fn anchor(&self) -> PathBuf {
        PathBuf::from(format!("/proc/self/fd/{}", self.handle.as_raw_fd()))
    }

    pub fn sync(&self) -> Result<()> {
        self.handle.sync_all()?;
        Ok(())
    }

    pub fn verify(&self) -> Result<()> {
        if normalize(&self.original)? != self.path {
            return Err(Error::Invalid("directory alias changed".into()));
        }
        check_overlap(&self.path, &self.forbidden)?;
        let current = walk(&self.path, false)?;
        let a = self.handle.metadata()?;
        let b = current.metadata()?;
        if (a.dev(), a.ino()) != (b.dev(), b.ino()) {
            return Err(Error::Invalid("directory identity changed".into()));
        }
        private(&b, true)
    }

    pub fn file(&self, name: &str, create: bool) -> Result<PathBuf> {
        self.verify()?;
        let path = self.path.join(name);
        if create {
            create_file(&self.handle, name)?;
        }
        self.check_file(name)?;
        self.verify()?;
        Ok(path)
    }

    pub fn check_file(&self, name: &str) -> Result<()> {
        let path = self.path.join(name);
        match fs::symlink_metadata(path) {
            Ok(m) => private(&m, false),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

fn private(meta: &fs::Metadata, directory: bool) -> Result<()> {
    // SAFETY: geteuid has no pointer arguments.
    let uid = unsafe { libc::geteuid() };
    let kind_ok = if directory {
        meta.is_dir()
    } else {
        meta.is_file() && meta.nlink() == 1
    };
    let mode = if directory { 0o700 } else { 0o600 };
    if !kind_ok || meta.uid() != uid || meta.mode() & 0o777 != mode {
        return Err(Error::Invalid(format!(
            "light storage must be owned by current user with mode {mode:o}"
        )));
    }
    Ok(())
}

fn create_file(parent: &File, name: &str) -> Result<()> {
    let name = CString::new(name).map_err(|_| Error::Invalid("NUL in filename".into()))?;
    // SAFETY: live directory descriptor, terminated filename and O_CREAT mode.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    if fd < 0 {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::AlreadyExists {
            return Ok(());
        }
        return Err(error.into());
    }
    // SAFETY: this newly created fd is transferred exactly once.
    let file = unsafe { File::from_raw_fd(fd) };
    file.sync_all()?;
    Ok(())
}

fn walk(path: &Path, create: bool) -> Result<File> {
    let mut parent = File::open("/")?;
    for component in path.components() {
        if let Component::Normal(name) = component {
            let name =
                CString::new(name.as_bytes()).map_err(|_| Error::Invalid("NUL in path".into()))?;
            parent = descend(&parent, &name, create)?;
        }
    }
    Ok(parent)
}

fn descend(parent: &File, name: &CString, create: bool) -> Result<File> {
    match open_at(parent, name) {
        Ok(f) => Ok(f),
        Err(e) if create && e.kind() == io::ErrorKind::NotFound => {
            // SAFETY: parent is a live descriptor, name a valid C string.
            let rc = unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) };
            if rc != 0 && io::Error::last_os_error().kind() != io::ErrorKind::AlreadyExists {
                return Err(io::Error::last_os_error().into());
            }
            Ok(open_at(parent, name)?)
        }
        Err(e) => Err(e.into()),
    }
}

fn open_at(parent: &File, name: &CString) -> io::Result<File> {
    // SAFETY: valid directory descriptor and terminated name; no variadic mode needed.
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
    // SAFETY: successful openat returned a new descriptor, transferred exactly once.
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn local(directory: &File) -> Result<()> {
    let mut info = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: writable statfs buffer and live descriptor.
    if unsafe { libc::fstatfs(directory.as_raw_fd(), info.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error().into());
    }
    // SAFETY: successful fstatfs initialized the buffer.
    let kind = unsafe { info.assume_init() }.f_type;
    if [0x6969, 0xff534d42, 0xfe534d42, 0x517b, 0x65735546].contains(&kind) {
        return Err(Error::Invalid(
            "WAL requires local storage; network/FUSE filesystem rejected".into(),
        ));
    }
    Ok(())
}
