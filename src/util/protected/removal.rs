//! Caller-authorized document removal, with a persistable inode proof for retries.
use super::{is_document, DOCUMENT};
use crate::{util, FlowError, Result};
use serde::{Deserialize, Serialize};
use std::{
    ffi::CString,
    fs::{self, File, OpenOptions},
    os::{
        fd::AsRawFd,
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DocumentRemoval {
    pub path: PathBuf,
    pub dev: u64,
    pub ino: u64,
}

/// The caller must establish the exact task/instance ownership before preparing
/// this proof, and persist it before removal. No namespace or ordinary file is
/// eligible. Missing documents need no proof and are not created.
pub fn prepare_document_removal(path: &Path) -> Result<Option<DocumentRemoval>> {
    if missing(path)? {
        return Ok(None);
    }
    if !path.is_absolute() || !is_document(path)? {
        return Err(FlowError::guard(
            "removal requires an exact protected document",
        ));
    }
    let directory = open_directory(path)?;
    validate_entries(&directory)?;
    let metadata = directory.metadata()?;
    let proof = DocumentRemoval {
        path: path.to_path_buf(),
        dev: metadata.dev(),
        ino: metadata.ino(),
    };
    verify_named(&proof)?;
    Ok(Some(proof))
}

/// Convenience for a single attempt. Crash-recoverable callers must instead
/// persist prepare_document_removal's result and pass it to resume.
pub fn remove_document(path: &Path) -> Result<()> {
    if let Some(proof) = prepare_document_removal(path)? {
        resume_document_removal(&proof)?;
    }
    Ok(())
}

/// Delete only the recorded directory. A missing payload/marker is allowed on
/// retry because the persisted inode proof predates those removals. Unknown
/// entries, path replacements, symlinks and ordinary files are never adopted.
pub fn resume_document_removal(proof: &DocumentRemoval) -> Result<()> {
    if !proof.path.is_absolute() {
        return Err(FlowError::guard("document removal path must be absolute"));
    }
    if missing(&proof.path)? {
        return Ok(());
    }
    let parent_path = proof
        .path
        .parent()
        .ok_or_else(|| FlowError::guard("missing parent"))?;
    let parent = open_directory(parent_path)?;
    let name = proof
        .path
        .file_name()
        .ok_or_else(|| FlowError::guard("missing name"))?;
    let (directory, anchored) = verified_child(proof, &parent, name)?;
    unlink_document(proof, &parent, name, &directory, &anchored)
}

fn missing(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(false),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(e) => Err(e.into()),
    }
}

fn open_directory(path: &Path) -> Result<File> {
    let directory = util::fs::secure::directory(path, false)?;
    let metadata = directory.metadata()?;
    if metadata.uid() != util::process::current_uid() || metadata.mode() & 0o022 != 0 {
        return Err(FlowError::guard("document directory is not private"));
    }
    Ok(directory)
}

fn descriptor_path(directory: &File) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()))
}

fn verify_named(proof: &DocumentRemoval) -> Result<()> {
    verify_metadata(proof, &fs::symlink_metadata(&proof.path)?)
}

fn verify_metadata(proof: &DocumentRemoval, metadata: &fs::Metadata) -> Result<()> {
    if !metadata.is_dir()
        || metadata.uid() != util::process::current_uid()
        || metadata.mode() & 0o022 != 0
        || (metadata.dev(), metadata.ino()) != (proof.dev, proof.ino)
    {
        return Err(FlowError::guard("protected document identity changed"));
    }
    Ok(())
}

fn validate_entries(directory: &File) -> Result<()> {
    for entry in fs::read_dir(descriptor_path(directory))? {
        let entry = entry?;
        let name = entry.file_name();
        if name != DOCUMENT && name != "authority.json" {
            return Err(FlowError::guard(
                "document contains namespace or foreign entries",
            ));
        }
        validate_entry_file(&entry.path())?;
    }
    Ok(())
}

fn unlink_child(directory: &File, name: &str, flags: i32) -> Result<()> {
    let name = CString::new(name).unwrap();
    unlink(directory, &name, flags)
}

fn unlink(directory: &File, name: &CString, flags: i32) -> Result<()> {
    // SAFETY: live directory fd, NUL-terminated single name, fixed unlink flags.
    if unsafe { libc::unlinkat(directory.as_raw_fd(), name.as_ptr(), flags) } == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.kind() == std::io::ErrorKind::NotFound {
        return Ok(());
    }
    Err(error.into())
}

fn verified_child(
    proof: &DocumentRemoval,
    parent: &File,
    name: &std::ffi::OsStr,
) -> Result<(File, PathBuf)> {
    let anchored = descriptor_path(parent).join(name);
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&anchored)?;
    verify_metadata(proof, &directory.metadata()?)?;
    validate_entries(&directory)?;
    Ok((directory, anchored))
}

fn unlink_document(
    proof: &DocumentRemoval,
    parent: &File,
    name: &std::ffi::OsStr,
    directory: &File,
    anchored: &Path,
) -> Result<()> {
    unlink_child(directory, "authority.json", 0)?;
    unlink_child(directory, DOCUMENT, 0)?;
    directory.sync_all()?;
    verify_metadata(proof, &fs::symlink_metadata(anchored)?)?;
    use std::os::unix::ffi::OsStrExt;
    let name = CString::new(name.as_bytes()).map_err(|_| FlowError::guard("invalid name"))?;
    unlink(parent, &name, libc::AT_REMOVEDIR)?;
    parent.sync_all()?;
    Ok(())
}

fn validate_entry_file(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file()
        || metadata.uid() != util::process::current_uid()
        || metadata.mode() & 0o077 != 0
        || metadata.nlink() != 1
    {
        return Err(FlowError::guard(
            "document entry is not a private regular file",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn document(path: &Path) {
        super::super::create(path).unwrap();
        util::atomic_write_json(path, &json!({"owned":true})).unwrap();
    }

    #[test]
    fn exact_document_retry_and_replacement_identity() {
        let temp = util::fs::test_tempdir();
        let path = temp.path().join("value.json");
        assert!(prepare_document_removal(&path).unwrap().is_none());
        document(&path);
        let proof = prepare_document_removal(&path).unwrap().unwrap();
        fs::rename(&path, temp.path().join("old")).unwrap();
        document(&path);
        assert!(resume_document_removal(&proof).is_err());
        assert_eq!(util::read_json(&path).unwrap(), json!({"owned":true}));
        let current = prepare_document_removal(&path).unwrap().unwrap();
        fs::remove_file(path.join("authority.json")).unwrap();
        fs::remove_file(path.join(DOCUMENT)).unwrap();
        assert!(prepare_document_removal(&path).is_err());
        resume_document_removal(&current).unwrap();
        resume_document_removal(&current).unwrap();
        assert!(!path.exists());
        assert!(temp.path().join("old").exists());
    }

    #[test]
    fn foreign_entries_and_symlinks_are_not_deleted() {
        let temp = util::fs::test_tempdir();
        let path = temp.path().join("value.json");
        document(&path);
        let proof = prepare_document_removal(&path).unwrap().unwrap();
        fs::write(path.join("foreign"), "preserve").unwrap();
        assert!(prepare_document_removal(&path).is_err());
        assert!(resume_document_removal(&proof).is_err());
        assert!(path.join("authority.json").exists());
        fs::remove_file(path.join("foreign")).unwrap();
        super::super::namespace(&path).unwrap();
        assert!(prepare_document_removal(&path).is_err());
        assert!(resume_document_removal(&proof).is_err());
        let link = temp.path().join("link");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(remove_document(&link).is_err());
        let ordinary = temp.path().join("ordinary");
        fs::write(&ordinary, "preserve").unwrap();
        assert!(remove_document(&ordinary).is_err());
        assert_eq!(fs::read_to_string(ordinary).unwrap(), "preserve");
    }
}
