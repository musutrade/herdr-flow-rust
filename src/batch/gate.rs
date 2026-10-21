//! Narrow client boundary for a separately deployed trusted gate.
//!
//! This is a consumer adapter protocol, not GateHouse's native API or an
//! evidence format. Deployment supplies a client that independently queries the
//! trusted service. Developer reports and arbitrary JSON files never grant
//! publication.

use std::path::Path;
use std::time::Duration;

use serde_json::{json, Value};

use crate::error::{FlowError, Result};
use crate::logging;
use crate::model::{self, J};
use crate::progress;
use crate::util;
mod executable;

pub fn validate(config: Option<&J>) -> Result<J> {
    let config = config
        .filter(|value| value.is_object())
        .ok_or_else(|| FlowError::progress("gate client configuration must be an object"))?;
    let verifier = model::get(config, "verifier").and_then(Value::as_array);
    let verifier_ok = verifier
        .map(|items| {
            (1..=32).contains(&items.len())
                && items.iter().all(|item| {
                    item.as_str()
                        .map(|text| !text.is_empty() && text.chars().count() <= 4000)
                        .unwrap_or(false)
                })
                && items
                    .first()
                    .and_then(Value::as_str)
                    .map(|first| Path::new(first).is_absolute())
                    .unwrap_or(false)
        })
        .unwrap_or(false);
    if !verifier_ok {
        return Err(FlowError::progress(
            "gate verifier must be an explicit argv with an absolute executable",
        ));
    }
    for key in ["policy", "check_name"] {
        progress::text(model::get(config, key), &format!("gate {key}"), 500)?;
    }
    let app_id = model::int(config, "app_id").unwrap_or(0);
    if app_id <= 0 {
        return Err(FlowError::progress(
            "gate requires the actual positive GitHub App ID",
        ));
    }
    Ok(config.clone())
}

pub fn binding(lane: &J, batch: &J) -> Result<J> {
    let snapshot = model::get(batch, "snapshot")
        .ok_or_else(|| FlowError::progress("batch snapshot is unavailable"))?;
    Ok(json!({
        "repo": lane.get("repo").cloned().unwrap_or(Value::Null),
        "host": lane.get("host").cloned().unwrap_or(Value::Null),
        "base_ref": lane.get("base_ref").cloned().unwrap_or(Value::Null),
        "base": batch.get("base").cloned().unwrap_or(Value::Null),
        "head": snapshot.get("head").cloned().unwrap_or(Value::Null),
        "tree": snapshot.get("tree").cloned().unwrap_or(Value::Null),
        "policy": lane.get("gate").and_then(|gate| gate.get("policy")).cloned().unwrap_or(Value::Null)
    }))
}

pub fn query(lane: &J, batch: &J, reference: Option<&J>, now: f64) -> Result<J> {
    let config = validate(model::get(lane, "gate"))?;
    query_binding(lane, batch, reference, now, config)
}

fn query_binding(lane: &J, batch: &J, reference: Option<&J>, now: f64, config: J) -> Result<J> {
    let expected = binding(lane, batch)?;
    query_reference(batch, reference, now, config, expected)
}

fn query_reference(
    batch: &J,
    reference: Option<&J>,
    now: f64,
    config: J,
    expected: J,
) -> Result<J> {
    let reference = progress::text(reference, "trusted gate run reference", 500)?;
    let request = query_request(batch, &expected, &reference);
    query_call(&config, &request, &expected, now)
}

fn query_request(batch: &J, expected: &J, reference: &str) -> J {
    json!({
        "version": 1,
        "action": "verify",
        "binding": expected,
        "operation": batch.get("gate").and_then(|gate| gate.get("operation")).cloned().unwrap_or(Value::Null),
        "reference": reference
    })
}

fn query_call(config: &J, request: &J, expected: &J, now: f64) -> Result<J> {
    let receipt = call(config, request)?;
    query_receipt(&receipt, request, expected, config, now)
}

fn query_receipt(receipt: &J, request: &J, expected: &J, config: &J, now: f64) -> Result<J> {
    if receipt_ok(receipt, request, expected, config, now) {
        query_fields(receipt)
    } else {
        Err(FlowError::progress(
            "trusted gate result is invalid, expired, incomplete or for different inputs",
        ))
    }
}

fn query_fields(receipt: &J) -> Result<J> {
    let fields = ["run", "external_id", "evidence"];
    fields
        .iter()
        .try_for_each(|key| query_field(receipt, key))?;
    Ok(receipt.clone())
}

fn query_field(receipt: &J, key: &str) -> Result<()> {
    progress::text(
        model::get(receipt, key),
        &format!("trusted gate {key}"),
        1000,
    )?;
    Ok(())
}

fn receipt_ok(receipt: &J, request: &J, expected: &J, config: &J, now: f64) -> bool {
    [
        receipt_version(receipt),
        receipt_binding(receipt, expected),
        receipt_operation(receipt, request),
        receipt_true_flag(receipt, "valid"),
        receipt_true_flag(receipt, "coverage_complete"),
        receipt_app_id(receipt, config),
        receipt_check_name(receipt, config),
        receipt_decision(receipt),
        receipt_unexpired(receipt, now),
    ]
    .into_iter()
    .all(|ok| ok)
}

fn receipt_version(receipt: &J) -> bool {
    model::get(receipt, "version").and_then(Value::as_i64) == Some(1)
}

fn receipt_binding(receipt: &J, expected: &J) -> bool {
    model::get(receipt, "binding") == Some(expected)
}

fn receipt_operation(receipt: &J, request: &J) -> bool {
    model::get(receipt, "operation") == model::get(request, "operation")
}

fn receipt_true_flag(receipt: &J, key: &str) -> bool {
    model::get(receipt, key) == Some(&json!(true))
}

fn receipt_app_id(receipt: &J, config: &J) -> bool {
    model::get(receipt, "app_id").and_then(Value::as_i64)
        == Some(model::int(config, "app_id").unwrap_or(0))
}

fn receipt_check_name(receipt: &J, config: &J) -> bool {
    model::string(receipt, "check_name") == model::string(config, "check_name")
}

fn receipt_decision(receipt: &J) -> bool {
    matches!(
        model::string(receipt, "decision").as_deref(),
        Some("allow" | "deny")
    )
}

fn receipt_unexpired(receipt: &J, now: f64) -> bool {
    model::number(receipt, "expires_at")
        .map(|value| value.is_finite() && value > now)
        .unwrap_or(false)
}

pub fn call(config: &J, request: &J) -> Result<J> {
    let Some(verifier) = model::get(config, "verifier").and_then(Value::as_array) else {
        return Err(FlowError::progress(
            "gate verifier must be an explicit argv with an absolute executable",
        ));
    };
    let argv: Vec<String> = verifier
        .iter()
        .filter_map(Value::as_str)
        .map(ToString::to_string)
        .collect();
    let stdin = serde_json::to_vec(request)?;
    call_run(&argv, stdin)
}

fn call_run(argv: &[String], stdin: Vec<u8>) -> Result<J> {
    let mut options = call_options(stdin);
    let program = argv
        .first()
        .ok_or_else(|| FlowError::progress("missing gate verifier executable"))?;
    options.executable_file = Some(executable::open(Path::new(program))?);
    let output = util::process::run_command(argv, &options).map_err(|error| {
        FlowError::progress(format!(
            "trusted gate verification unavailable; preserve the existing operation: {error}"
        ))
    })?;
    call_output(output)
}

fn call_options(stdin: Vec<u8>) -> util::process::CommandOptions {
    util::process::CommandOptions {
        cwd: Some(std::path::PathBuf::from("/")),
        timeout: Duration::from_secs(45),
        stdin: Some(stdin),
        stdout_limit: 65_536,
        stderr_limit: 65_536,
        ..Default::default()
    }
}

fn call_output(output: util::process::CommandOutput) -> Result<J> {
    if call_output_invalid(&output) {
        logging::log_event_fields(
            "gate_verifier_failed",
            "gate_client",
            "trusted gate verifier returned an invalid result",
            logging::Fields {
                metadata: Some(&json!({
                    "returncode": output.status,
                    "stdout_bytes": output.stdout.len(),
                    "stderr_bytes": output.stderr.len()
                })),
                ..Default::default()
            },
        );
        return Err(FlowError::progress(
            "trusted gate verifier failed; preserve the existing operation",
        ));
    }
    call_parse(&output.stdout)
}

fn call_output_invalid(output: &util::process::CommandOutput) -> bool {
    output.status != 0 || output.stdout.len() > 65_536
}

fn call_parse(stdout: &[u8]) -> Result<J> {
    let receipt: J = serde_json::from_slice(stdout)
        .map_err(|_| FlowError::progress("trusted gate verifier returned invalid JSON"))?;
    call_object(receipt)
}

fn call_object(receipt: J) -> Result<J> {
    if receipt.is_object() {
        Ok(receipt)
    } else {
        Err(FlowError::progress(
            "trusted gate response must be an object",
        ))
    }
}

/// Inspect an uncertain operation without claiming quality or rerunning it.
pub fn quiescent(lane: &J, batch: &J) -> Result<J> {
    let config = validate(model::get(lane, "gate"))?;
    quiescent_binding(lane, batch, config)
}

fn quiescent_binding(lane: &J, batch: &J, config: J) -> Result<J> {
    let expected = binding(lane, batch)?;
    quiescent_request(batch, config, expected)
}

fn quiescent_request(batch: &J, config: J, expected: J) -> Result<J> {
    let request = json!({
        "version": 1,
        "action": "inspect",
        "binding": expected,
        "operation": batch.get("gate").and_then(|gate| gate.get("operation")).cloned().unwrap_or(Value::Null)
    });
    quiescent_call(config, request)
}

fn quiescent_call(config: J, request: J) -> Result<J> {
    let response = call(&config, &request)?;
    quiescent_check(response, &request)
}

fn quiescent_check(response: J, request: &J) -> Result<J> {
    if quiescent_ok(&response, request) {
        quiescent_evidence(response)
    } else {
        Err(FlowError::progress(
            "trusted gate operation is still running or uncertain; preserve its inputs",
        ))
    }
}

fn quiescent_evidence(response: J) -> Result<J> {
    progress::text(
        model::get(&response, "evidence"),
        "trusted operation inspection evidence",
        1000,
    )?;
    Ok(response)
}

fn quiescent_ok(response: &J, request: &J) -> bool {
    [
        receipt_version(response),
        request_binding(response, request),
        request_operation(response, request),
        inspection_status(response),
        receipt_true_flag(response, "quiescent"),
    ]
    .into_iter()
    .all(|ok| ok)
}

fn request_binding(response: &J, request: &J) -> bool {
    model::get(response, "binding") == model::get(request, "binding")
}

fn request_operation(response: &J, request: &J) -> bool {
    model::get(response, "operation") == model::get(request, "operation")
}

fn inspection_status(response: &J) -> bool {
    matches!(
        model::string(response, "status").as_deref(),
        Some("completed" | "failed" | "cancelled" | "never_started")
    )
}
