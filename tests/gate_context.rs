//! Isolated inherited-environment probe, with no real credentials or service.
pub mod common;
use herdr_flow::batch::gate;
use serde_json::json;
use std::{fs, process::Command};

#[test]
fn inherited_shell_startup_file_cannot_replace_verifier_result() {
    const CHILD: &str = "HERDR_GATE_CONTEXT_PROBE";
    if std::env::var_os(CHILD).is_some() {
        let value = gate::call(
            &json!({"verifier":["/bin/bash","-c","printf '{\"trusted\":true}'"]}),
            &json!({"action":"verify"}),
        )
        .unwrap();
        assert_eq!(value, json!({"trusted":true}));
        let environment = gate::call(
            &json!({"verifier":["/bin/sh","-c",
                "test -z \"${HOME+x}${HERDR_FAKE_SERVICE_TOKEN+x}${PYTHONPATH+x}${BASH_ENV+x}\" && test \"$PATH\" = /usr/bin:/bin && test \"$LANG\" = C && test \"$PWD\" = / && printf '{\"clean\":true}'"]}),
            &json!({"action":"inspect"}),
        )
        .unwrap();
        assert_eq!(environment, json!({"clean":true}));
        return;
    }
    let root = common::tempdir().unwrap();
    let startup = root.path().join("startup");
    fs::write(&startup, "printf '{\"forged\":true}'; exit 0\n").unwrap();
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "inherited_shell_startup_file_cannot_replace_verifier_result",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .env("BASH_ENV", &startup)
        .env("HOME", root.path())
        .env("PATH", root.path())
        .env("LANG", "caller-controlled")
        .env("PYTHONPATH", root.path())
        .env("HERDR_FAKE_SERVICE_TOKEN", "fixture-only")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
}
