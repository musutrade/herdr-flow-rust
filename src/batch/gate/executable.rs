//! Trusted installation checks and a stable descriptor for this invocation.
use crate::{util, FlowError, Result};
use std::{
    fs::{self, File, Metadata, OpenOptions},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::Path,
    sync::Arc,
};

pub(super) fn open(path: &Path) -> Result<Arc<File>> {
    if !path.is_absolute() {
        return Err(FlowError::progress(
            "gate verifier executable must be absolute",
        ));
    }
    check_path(path)?;
    let resolved = fs::canonicalize(path)?;
    check_path(&resolved)?;
    check_executable(&fs::symlink_metadata(&resolved)?)?;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&resolved)?;
    check_executable(&file.metadata()?)?;
    Ok(Arc::new(file))
}

fn check_path(path: &Path) -> Result<()> {
    for component in path.ancestors() {
        let metadata = fs::symlink_metadata(component)?;
        check_owner(&metadata)?;
        if metadata.file_type().is_symlink() {
            continue;
        }
        let writable = metadata.mode() & 0o022 != 0;
        let protected_sticky_directory = metadata.is_dir() && metadata.mode() & libc::S_ISVTX != 0;
        if writable && !protected_sticky_directory {
            return Err(FlowError::progress(
                "gate verifier installation must not be writable by group or others",
            ));
        }
    }
    Ok(())
}

fn check_owner(metadata: &Metadata) -> Result<()> {
    if metadata.uid() != 0 && metadata.uid() != util::process::current_uid() {
        return Err(FlowError::progress(
            "gate verifier installation must be owned by root or the invoking user",
        ));
    }
    Ok(())
}

fn check_executable(metadata: &Metadata) -> Result<()> {
    check_owner(metadata)?;
    if !metadata.is_file() || metadata.mode() & 0o111 == 0 || metadata.mode() & 0o022 != 0 {
        return Err(FlowError::progress(
            "gate verifier must be a regular executable without group/other write permission",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::PermissionsExt;

    fn script(path: &Path, result: &str) {
        fs::write(path, format!("#!/bin/sh\nprintf '{result}'\n")).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[test]
    fn opened_script_survives_atomic_upgrade_and_fresh_calls_use_new_identity() {
        let dir = util::fs::test_tempdir();
        let path = dir.path().join("verifier");
        let link = dir.path().join("current");
        script(&path, "old");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        let file = open(&link).unwrap();
        let replacement = dir.path().join("replacement");
        script(&replacement, "new");
        fs::rename(&replacement, &path).unwrap();
        let argv = vec![link.to_str().unwrap().to_owned()];
        let options = util::process::CommandOptions {
            executable_file: Some(file.clone()),
            ..Default::default()
        };
        let first = util::process::run_command(&argv, &options).unwrap();
        assert!(first.success());
        assert_eq!(first.stdout, b"old");
        // SAFETY: fcntl reads flags from the descriptor owned by file.
        let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFD) };
        assert_ne!(flags & libc::FD_CLOEXEC, 0);
        let fresh = util::process::CommandOptions {
            executable_file: Some(open(&link).unwrap()),
            ..Default::default()
        };
        let second = util::process::run_command(&argv, &fresh).unwrap();
        assert!(second.success());
        assert_eq!(second.stdout, b"new");
    }

    #[test]
    fn directories_nonexecutables_and_symlinks_to_writable_code_are_rejected() {
        let dir = util::fs::test_tempdir();
        assert!(open(dir.path()).is_err());
        let path = dir.path().join("program");
        fs::write(&path, "data").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(open(&path).is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o777)).unwrap();
        let link = dir.path().join("current");
        std::os::unix::fs::symlink(path, &link).unwrap();
        assert!(open(&link).is_err());
        assert!(open(Path::new("relative-verifier")).is_err());
    }
}
