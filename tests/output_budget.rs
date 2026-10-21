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

// The child publishes its own identity before the producer can exceed the budget.
// Check that identity after capture, rather than assuming the parent gets CPU
// before a delayed child writes a marker. Zombies have exited even if init has
// not reaped them yet.
fn process_identity(stat: &str) -> (&str, &str, &str) {
    let (pid, _) = stat.split_once(' ').unwrap();
    let (_, fields) = stat.rsplit_once(')').unwrap();
    let fields: Vec<_> = fields.split_whitespace().collect();
    (pid, fields[19], fields[0])
}

fn same_process_running(record: &str) -> bool {
    let (pid, ticks, _) = process_identity(record);
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(current) => {
            let (_, current_ticks, state) = process_identity(&current);
            ticks == current_ticks && !matches!(state, "Z" | "X")
        }
        Err(error)
            if error.kind() == std::io::ErrorKind::NotFound
                || error.raw_os_error() == Some(libc::ESRCH) =>
        {
            false
        }
        Err(error) => panic!("cannot inspect fixture process {pid}: {error}"),
    }
}

#[test]
fn output_limit_reclaims_group_and_preserves_unrelated_process() {
    // Delayed producer reproduces the old marker test's false failure without
    // relying on CPU load. The descendant remains alive until group cleanup.
    for producer_delay in ["0", "0.5"] {
        let directory = tempfile::tempdir().unwrap();
        let ready = directory.path().join("child.stat");
        let temporary = directory.path().join("child.stat.tmp");
        let quote = herdr_flow::util::shell_quote;
        let child = format!(
            "cat /proc/$$/stat > {} && mv {} {}; exec sleep 10",
            quote(temporary.to_str().unwrap()),
            quote(temporary.to_str().unwrap()),
            quote(ready.to_str().unwrap()),
        );
        let script = format!(
            "sh -c {} & while [ ! -s {} ]; do sleep 0.01; done; sleep {}; yes output",
            quote(&child),
            quote(ready.to_str().unwrap()),
            producer_delay,
        );
        let mut unrelated = Command::new("sleep").arg("10").spawn().unwrap();
        let unrelated_record =
            std::fs::read_to_string(format!("/proc/{}/stat", unrelated.id())).unwrap();
        let result = capture(&script, 1024);
        let alive =
            unrelated.try_wait().unwrap().is_none() && same_process_running(&unrelated_record);
        let _ = unrelated.kill();
        unrelated.wait().unwrap();
        assert_eq!(result.unwrap_err().kind, ErrorKind::OutputLimit);
        assert!(alive, "unrelated process was killed");
        let record = std::fs::read_to_string(&ready).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while same_process_running(&record) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            !same_process_running(&record),
            "owned descendant survived cleanup"
        );
    }
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
