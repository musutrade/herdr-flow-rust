//! Verifier installation permissions are checked before executing any bytes.
pub mod common;
use herdr_flow::batch::gate;
use serde_json::json;
use std::{fs, os::unix::fs::PermissionsExt};

#[test]
fn writable_verifier_or_parent_is_rejected_before_execution() {
    for (file_mode, directory_mode) in [(0o777, 0o700), (0o700, 0o777)] {
        let root = common::tempdir().unwrap();
        let install = root.path().join("install");
        fs::create_dir(&install).unwrap();
        let program = install.join("verify");
        let executed = root.path().join("executed");
        fs::write(
            &program,
            format!(
                "#!/bin/sh\nprintf executed > {}\nprintf '{{}}'\n",
                herdr_flow::util::shell_quote(executed.to_str().unwrap())
            ),
        )
        .unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(file_mode)).unwrap();
        fs::set_permissions(&install, fs::Permissions::from_mode(directory_mode)).unwrap();
        let result = gate::call(&json!({"verifier":[program]}), &json!({}));
        assert!(
            result.is_err(),
            "untrusted installation executed: {result:?}"
        );
        assert!(!executed.exists());
    }
}
