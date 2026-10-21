//! Isolated output-budget regressions; no services or user task state.
use herdr_flow::util::process::{run_command, CommandOptions};
use herdr_flow::{batch, ErrorKind};
use serde_json::json;
use std::process::Command;
use std::time::{Duration, Instant};

fn capture(script: &str, limit: usize) -> herdr_flow::Result<herdr_flow::util::CommandOutput> {
    run_command(
        &["/bin/sh".into(), "-c".into(), script.into()],
        &CommandOptions {
            timeout: Duration::from_secs(5),
            stdout_limit: limit,
            stderr_limit: limit,
            ..Default::default()
        },
    )
}

#[test]
fn exact_limits_and_empty_streams_succeed_without_truncation() {
    let output = capture("printf 1234; printf 5678 >&2; exit 7", 4).unwrap();
    assert_eq!(output.stdout, b"1234");
    assert_eq!(output.stderr, b"5678");
    assert_eq!(output.status, 7);
    assert!(capture("exit 0", 0).unwrap().success());
    assert_eq!(
        capture("printf x", 0).unwrap_err().kind,
        ErrorKind::OutputLimit
    );
}

#[test]
fn oversized_single_and_dual_streams_fail_promptly() {
    for script in [
        "printf 12345",
        "printf 12345 >&2",
        "yes secret-output",
        "yes secret-output >&2",
        "yes secret-output & yes secret-output >&2 & wait",
    ] {
        let start = Instant::now();
        let error = capture(script, 4).unwrap_err();
        assert_eq!(error.kind, ErrorKind::OutputLimit, "{script}");
        assert_eq!(error.kind.type_name(), "OutputLimitExceeded");
        assert!(!error.message.contains("secret-output"));
        assert!(start.elapsed() < Duration::from_secs(3));
    }
}

#[test]
fn output_limit_reclaims_group_and_preserves_unrelated_process() {
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("survived");
    let mut unrelated = Command::new("sleep").arg("10").spawn().unwrap();
    let script = format!(
        "(sleep 0.3; printf survived > {}) & yes output",
        herdr_flow::util::shell_quote(marker.to_str().unwrap())
    );
    let result = capture(&script, 1024);
    std::thread::sleep(Duration::from_millis(450));
    let alive = unrelated.try_wait().unwrap().is_none();
    let _ = unrelated.kill();
    unrelated.wait().unwrap();
    assert_eq!(result.unwrap_err().kind, ErrorKind::OutputLimit);
    assert!(!marker.exists());
    assert!(alive);
}

#[test]
fn verifier_limits_both_streams_before_parsing_receipts() {
    for script in ["yes sensitive", "yes sensitive >&2"] {
        let start = Instant::now();
        let error = batch::gate::call(
            &json!({"verifier":["/bin/sh","-c",script]}),
            &json!({"action":"verify"}),
        )
        .unwrap_err();
        assert_eq!(error.kind, ErrorKind::Progress);
        assert!(error.message.contains("65536 byte capture limit"));
        assert!(error.message.contains("preserve the existing operation"));
        assert!(!error.message.contains("sensitive"));
        assert!(start.elapsed() < Duration::from_secs(3));
    }
    assert_eq!(
        batch::gate::call(
            &json!({"verifier":["/bin/sh","-c","printf '{}' "]}),
            &json!({})
        )
        .unwrap(),
        json!({})
    );
}
