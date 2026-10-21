//! Probe process inheritance using only a temporary directory and owned child.
pub mod common;
use std::{fs::File, os::fd::FromRawFd, process::Command};

#[test]
fn state_directory_descriptor_is_not_inherited_across_exec() {
    let root = common::tempdir().unwrap();
    let raw = herdr_flow::util::open_dir_fd(root.path()).unwrap();
    // SAFETY: open_dir_fd returned a fresh owned descriptor. Transfer it once;
    // no other Rust owner closes it while the child checks inheritance.
    let _owner = unsafe { File::from_raw_fd(raw) };
    let child = Command::new("/bin/sh")
        .args(["-c", "! test \"$1\" -ef \"$2\"", "probe"])
        .arg(format!("/proc/self/fd/{raw}"))
        .arg(root.path())
        .env_clear()
        .output()
        .unwrap();
    assert!(
        child.status.success(),
        "state directory fd was inherited: {child:?}"
    );
}
