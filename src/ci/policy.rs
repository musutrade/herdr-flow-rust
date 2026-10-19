//! GitHub Actions decision policy. Observations stay complete and read-only.

use std::collections::BTreeSet;

use serde_json::{json, Value};

use crate::error::Result;
use crate::model::{self, J};
use crate::util::pyjson::python_sorted_compact;

/// Decide from exact required names, independently of advisory workflow work.
pub fn required_jobs(spec: &J, run: &J, jobs: &[J]) -> Result<J> {
    let names: BTreeSet<String> = model::array_field(spec, "jobs")
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(ToString::to_string)
                .collect()
        })
        .unwrap_or_default();
    let selected: Vec<&J> = jobs
        .iter()
        .filter(|job| {
            model::string(job, "name")
                .map(|name| names.contains(&name))
                .unwrap_or(false)
        })
        .collect();
    let selected_names: BTreeSet<String> = selected
        .iter()
        .filter_map(|job| model::string(job, "name"))
        .collect();
    let missing: Vec<String> = names.difference(&selected_names).cloned().collect();
    let completed: Vec<&&J> = selected
        .iter()
        .filter(|job| model::string(job, "status").as_deref() == Some("completed"))
        .collect();
    let all_completed = selected
        .iter()
        .all(|job| model::string(job, "status").as_deref() == Some("completed"));
    let run_completed = model::string(run, "status").as_deref() == Some("completed");
    let terminal = all_completed && (missing.is_empty() || run_completed);
    let conclusions: Vec<String> = completed
        .iter()
        .filter_map(|job| model::string(job, "conclusion"))
        .collect();
    let (state, reason) = if completed.iter().any(|job| {
        !matches!(
            model::string(job, "conclusion").as_deref(),
            Some("success" | "failure")
        )
    }) {
        ("needs_attention", "required_job_not_successful")
    } else if !missing.is_empty() && run_completed {
        ("needs_attention", "required_job_missing")
    } else if conclusions.iter().any(|value| value == "failure") {
        ("failure", "required_job_failed")
    } else if !missing.is_empty() || completed.len() != selected.len() {
        ("running", "required_jobs_pending")
    } else {
        ("success", "required_jobs_succeeded")
    };
    let _ = python_sorted_compact(&json!({}));
    Ok(json!({
        "state": state,
        "decision_reason": reason,
        "decision_terminal": terminal
    }))
}
