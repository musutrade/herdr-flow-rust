//! State fixtures are private independently of the invoking shell's umask.
use std::fs::{DirBuilder, Permissions};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::Path;

pub fn tempdir() -> std::io::Result<tempfile::TempDir> {
    tempfile::Builder::new()
        .permissions(Permissions::from_mode(0o700))
        .tempdir()
}

pub fn create_dir(path: impl AsRef<Path>) -> std::io::Result<()> {
    DirBuilder::new().mode(0o700).create(path)
}

pub fn create_dir_all(path: impl AsRef<Path>) -> std::io::Result<()> {
    DirBuilder::new().recursive(true).mode(0o700).create(path)
}
