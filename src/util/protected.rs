//! Opt-in document directories: legacy file readers and atomic file replacements
//! cannot consume or overwrite their authority. Never converts existing files.
mod removal;
use crate::{FlowError, Result};
pub use removal::{
    prepare_document_removal, remove_document, resume_document_removal, DocumentRemoval,
};
use std::{
    fs,
    io::{Read, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
};
const DOCUMENT: &str = ".flow-document-v2";
const NAMESPACE: &str = ".flow-namespace-v2";
const RUNTIME: &str = ".flow-runtime-v2";
pub(crate) const FORMAT: &[u8] = b"herdr-flow isolated authority 3\n";

fn marker(root: &Path, name: &str) -> Result<bool> {
    let path = root.join(name);
    let mut f = match fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(&path)
    {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    super::ensure_private_dir(root, false)?;
    if !f.metadata()?.is_file() {
        return Err(FlowError::guard("invalid authority marker"));
    }
    let mut bytes = Vec::new();
    Read::by_ref(&mut f).take(128).read_to_end(&mut bytes)?;
    if bytes != FORMAT {
        return Err(FlowError::guard("unknown authority format"));
    }
    Ok(true)
}

fn initialize(root: &Path, name: &str) -> Result<()> {
    if root.try_exists()? && marker(root, name)? {
        return Ok(());
    }
    reserve_directory(root)?;
    write_marker(root, name)
}

fn reserve_directory(root: &Path) -> Result<()> {
    match fs::symlink_metadata(root) {
        Ok(m) if m.is_dir() && fs::read_dir(root)?.next().is_none() => (),
        Ok(_) => {
            return Err(FlowError::guard(
                "isolated authority requires a new empty directory; no migration",
            ))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            create_directory(root)?;
        }
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

fn create_directory(root: &Path) -> Result<()> {
    let parent = root
        .parent()
        .ok_or_else(|| FlowError::guard("missing authority parent"))?;
    super::ensure_private_dir(parent, false)?;
    fs::DirBuilder::new().mode(0o700).create(root)?;
    Ok(())
}

fn write_marker(root: &Path, name: &str) -> Result<()> {
    super::ensure_private_dir(root, false)?;
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(root.join(name))?;
    f.write_all(FORMAT)?;
    f.sync_all()?;
    fs::File::open(root)?.sync_all()?;
    Ok(())
}

/// Reserve a fresh JSON document name as a directory. Partial init fails closed.
pub fn create(path: &Path) -> Result<()> {
    initialize(path, DOCUMENT)
}
/// All subsequently written JSON documents in this fresh namespace are protected.
pub fn namespace(root: &Path) -> Result<()> {
    if is_document(root)? {
        // A document may also contain its private sidecars.
        if marker(root, NAMESPACE)? {
            return Ok(());
        }
        write_marker(root, NAMESPACE)?;
        return Ok(());
    }
    initialize(root, NAMESPACE)
}
pub fn is_document(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => marker(path, DOCUMENT),
        Ok(_) => Ok(false),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}
/// Pure path resolution for reads; no marker/directory creation.
pub fn read_path(path: &Path) -> Result<PathBuf> {
    if is_document(path)? {
        return Ok(path.join("authority.json"));
    }
    reject_unprotected_read(path)?;
    Ok(path.to_path_buf())
}
fn reject_unprotected_read(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        if is_namespace(parent)? && !is_payload(path, parent)? {
            match fs::symlink_metadata(path) {
                Ok(_) => {
                    return Err(FlowError::guard(
                        "unprotected document in isolated namespace",
                    ))
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
                Err(e) => return Err(e.into()),
            }
        }
    }
    Ok(())
}
pub fn write_path(path: &Path) -> Result<PathBuf> {
    if is_document(path)? {
        return Ok(path.join("authority.json"));
    }
    write_nested(path)
}
fn write_nested(path: &Path) -> Result<PathBuf> {
    let Some(parent) = path.parent() else {
        return Ok(path.to_path_buf());
    };
    // authority.json is the single payload of a document, not another wrapper.
    if is_payload(path, parent)? {
        return Ok(path.to_path_buf());
    }
    if is_namespace(parent)? {
        create(path)?;
        return Ok(path.join("authority.json"));
    }
    Ok(path.to_path_buf())
}

fn is_payload(path: &Path, parent: &Path) -> Result<bool> {
    Ok(path.file_name().is_some_and(|n| n == "authority.json") && is_document(parent)?)
}

/// Read-only namespace recognition.
pub fn is_namespace(root: &Path) -> Result<bool> {
    match fs::symlink_metadata(root) {
        Ok(m) if m.is_dir() => marker(root, NAMESPACE),
        Ok(_) => Ok(false),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

pub fn runtime_namespace(root: &Path) -> Result<()> {
    namespace(root)?;
    if marker(root, RUNTIME)? {
        return Ok(());
    }
    write_marker(root, RUNTIME)
}

pub fn is_runtime_namespace(root: &Path) -> Result<bool> {
    let runtime = marker(root, RUNTIME)?;
    if runtime && !is_namespace(root)? {
        return Err(FlowError::guard("runtime authority namespace is missing"));
    }
    Ok(runtime)
}

#[cfg(test)]
mod format_tests {
    use super::*;
    #[test]
    fn previous_format_is_refused_without_migration_or_rewrite() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::Builder::new()
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        namespace(temp.path()).unwrap();
        let marker = temp.path().join(NAMESPACE);
        assert_eq!(fs::read(&marker).unwrap(), FORMAT);
        let old = b"herdr-flow isolated authority 1\n";
        assert_ne!(FORMAT, old);
        fs::write(&marker, old).unwrap();
        assert!(is_namespace(temp.path()).is_err());
        assert!(namespace(temp.path()).is_err());
        assert!(write_path(&temp.path().join("fresh.json")).is_err());
        assert!(!temp.path().join("fresh.json").exists());
        assert_eq!(fs::read(&marker).unwrap(), old);
    }
}
