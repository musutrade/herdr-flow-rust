//! Read-only task and delivery metrics derived from durable queue records.
//!
//! Missing evidence stays `null` with a reason; it is never reported as zero.
//! Nothing here mutates the queue, dispatches work or reruns validation.

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::{json, Map, Value};

use crate::error::Result;
use crate::model::{self, J};
use crate::util;
use crate::watch_service;

fn timestamp(value: Option<&J>) -> Option<f64> {
    value
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite())
}

fn span(start: Option<f64>, end: Option<f64>, missing: &str) -> J {
    let (Some(start), Some(end)) = (start, end) else {
        return json!({"seconds": null, "reason": missing});
    };
    if end < start {
        return json!({"seconds": null, "reason": "clock anomaly: end precedes start"});
    }
    json!({
        "seconds": round3(end - start),
        "start": start,
        "end": end
    })
}

fn round3(value: f64) -> f64 {
    (value * 1000.0).round() / 1000.0
}

fn binding_matches(task: &J, event: &J) -> bool {
    let binding = model::get(event, "binding")
        .cloned()
        .unwrap_or_else(|| json!({}));
    ["task", "epoch", "assignment", "candidate"]
        .iter()
        .all(|key| model::get(task, key) == model::get(&binding, key))
}

fn delivery_index(s: &J) -> (BTreeMap<String, Vec<String>>, Vec<J>) {
    let mut mapping: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut duplicates = Vec::new();
    if let Some(deliveries) = model::get(s, "deliveries").and_then(Value::as_object) {
        for (delivery_id, delivery) in deliveries {
            if let Some(events) = model::array_field(delivery, "events") {
                for event_id in events.iter().filter_map(Value::as_str) {
                    let entry = mapping.entry(event_id.to_string()).or_default();
                    entry.push(delivery_id.clone());
                    if entry.len() > 1 {
                        duplicates.push(json!({"event": event_id, "deliveries": entry}));
                    }
                }
            }
        }
    }
    (mapping, duplicates)
}

fn delivery_metrics(delivery_id: Option<&str>, delivery: Option<&J>) -> J {
    let Some(delivery) = delivery else {
        return json!({
            "id": delivery_id,
            "phase": null,
            "sent_at": null,
            "ack_at": null,
            "segments": {"delivery_to_ack": span(None, None, "delivery not recorded")}
        });
    };
    json!({
        "id": delivery_id,
        "phase": delivery.get("phase").cloned().unwrap_or(Value::Null),
        "sent_at": timestamp(model::get(delivery, "sent_at")),
        "ack_at": timestamp(model::get(delivery, "ack_at")),
        "segments": {
            "delivery_to_ack": span(
                timestamp(model::get(delivery, "sent_at")),
                timestamp(model::get(delivery, "ack_at")),
                "ACK not observed"
            )
        }
    })
}

fn event_metrics(task: &J, event: &J, delivery: Option<&J>) -> J {
    let delivery_id = delivery.and_then(|delivery| model::string(delivery, "id"));
    let observed = timestamp(model::get(event, "observed_at"));
    let sent = timestamp(delivery.and_then(|delivery| model::get(delivery, "sent_at")));
    let ack = timestamp(delivery.and_then(|delivery| model::get(delivery, "ack_at")));
    let applied = if model::bool_or(event, "applied", false) {
        timestamp(model::get(event, "applied_at"))
    } else {
        None
    };
    json!({
        "id": event.get("id").cloned().unwrap_or(Value::Null),
        "wait": event.get("wait").cloned().unwrap_or(Value::Null),
        "outcome": event.get("outcome").cloned().unwrap_or(Value::Null),
        "kind": event.get("kind").cloned().unwrap_or(Value::Null),
        "observed_at": observed,
        "acknowledged": model::bool_or(event, "acknowledged", false),
        "applied": model::bool_or(event, "applied", false),
        "applied_at": applied,
        "archived": event.get("archived").cloned().unwrap_or(Value::Null),
        "current": binding_matches(task, event),
        "delivery": delivery_metrics(delivery_id.as_deref(), delivery),
        "segments": {
            "ready_to_delivery": span(observed, sent, "delivery not sent"),
            "delivery_to_ack": span(sent, ack, "ACK not observed"),
            "ack_to_applied": span(ack, applied, "result not applied"),
            "ready_to_applied": span(observed, applied, "result not applied")
        }
    })
}

fn operation_metrics(event_by_wait: &BTreeMap<String, J>, operation: &J) -> J {
    let prepared = timestamp(model::get(operation, "prepared_at"));
    let started = timestamp(model::get(operation, "started_at"));
    let submitted = timestamp(model::get(operation, "submitted_at"));
    let accepted = timestamp(model::get(operation, "accepted_at"));
    let intent = model::get(operation, "intent")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let mut next_event =
        model::string(&intent, "wait").and_then(|wait| event_by_wait.get(&wait).cloned());
    if let Some(event) = &next_event {
        let assignment = model::dig_str(event, &["binding", "assignment"]);
        if assignment.is_some() && assignment != model::string(operation, "assignment") {
            next_event = None;
        }
    }
    let mut next_observed = timestamp(
        next_event
            .as_ref()
            .and_then(|event| model::get(event, "observed_at")),
    );
    if let (Some(next), Some(accepted)) = (next_observed, accepted) {
        if next < accepted {
            next_observed = None;
        }
    }
    json!({
        "id": operation.get("id").cloned().unwrap_or(Value::Null),
        "assignment": operation.get("assignment").cloned().unwrap_or(Value::Null),
        "phase": operation.get("phase").cloned().unwrap_or(Value::Null),
        "prepared_at": prepared,
        "started_at": started,
        "submitted_at": submitted,
        "accepted_at": accepted,
        "deadline": timestamp(model::get(operation, "deadline")),
        "segments": {
            "prepared_to_started": span(prepared, started, "dispatch not started"),
            "started_to_submitted": span(started, submitted, "transport response not recorded"),
            "prepared_to_accepted": span(prepared, accepted, "takeover not accepted"),
            "accepted_to_next_result": span(accepted, next_observed, "next result not observed")
        }
    })
}

fn event_by_wait(events: &[J]) -> BTreeMap<String, J> {
    let mut result = BTreeMap::new();
    for event in events {
        insert_wait_event(&mut result, event);
    }
    result
}

fn insert_wait_event(result: &mut BTreeMap<String, J>, event: &J) {
    let Some(wait) = wait_event_candidate(event) else {
        return;
    };
    let replace = result
        .get(&wait)
        .map(|current| is_newer_event(event, current))
        .unwrap_or(true);
    if replace {
        result.insert(wait, event.clone());
    }
}

fn wait_event_candidate(event: &J) -> Option<String> {
    if model::string(event, "kind").as_deref() == Some("progress_stall") {
        return None;
    }
    model::string(event, "wait")
}

fn is_newer_event(event: &J, current: &J) -> bool {
    newer_event_time(
        timestamp(model::get(event, "observed_at")),
        timestamp(model::get(current, "observed_at")),
    )
}

fn newer_event_time(observed: Option<f64>, current: Option<f64>) -> bool {
    let Some(observed) = observed else {
        return false;
    };
    current.map(|current| observed < current).unwrap_or(true)
}

fn task_event_metrics(
    s: &J,
    task: &J,
    events: &[J],
    deliveries: &BTreeMap<String, Vec<String>>,
) -> Vec<J> {
    let mut result = Vec::new();
    for event in events {
        if model::string(event, "kind").as_deref() == Some("progress_stall") {
            continue;
        }
        let event_id = model::string(event, "id").unwrap_or_default();
        let delivery = deliveries
            .get(&event_id)
            .and_then(|ids| ids.first())
            .and_then(|id| model::dig(s, &["deliveries", id]));
        result.push(event_metrics(task, event, delivery));
    }
    result
}

fn task_segments(
    task: &J,
    first_result: Option<f64>,
    last_applied: Option<f64>,
    completed: Option<f64>,
    now: f64,
) -> J {
    let created = timestamp(model::get(task, "created_at"));
    let mut segments = json!({
        "created_to_first_result": span(created, first_result, "result not observed"),
        "created_to_completed": span(created, completed, "task not completed"),
        "last_applied_to_completed": span(last_applied, completed, "task not completed")
    });
    if completed.is_none() && created.is_some() {
        segments["ongoing_since_created"] = json!({
            "seconds": round3(now - created.unwrap_or(now)),
            "ongoing": true,
            "start": created,
            "end": now
        });
    }
    segments
}

fn counter(items: impl Iterator<Item = String>) -> Map<String, Value> {
    let mut result = BTreeMap::new();
    for item in items {
        *result.entry(item).or_insert(0i64) += 1;
    }
    result
        .into_iter()
        .map(|(key, value)| (key, json!(value)))
        .collect()
}

fn task_counts(
    task: &J,
    event_metrics: &[J],
    applied: &[J],
    stall_events: &[J],
    operations: &[J],
) -> J {
    let stall_reasons = counter(
        stall_events
            .iter()
            .map(|event| model::string(event, "reason").unwrap_or_else(|| "unknown".to_string())),
    );
    json!({
        "events": event_metrics.len(),
        "applied_events": applied.len(),
        "pending_events": model::array_field(task, "pending").map(Vec::len).unwrap_or(0),
        "archived_events": event_metrics.iter().filter(|event| !event.get("archived").map(Value::is_null).unwrap_or(true)).count(),
        "stale_events": event_metrics.iter().filter(|event| !model::bool_or(event, "current", false)).count(),
        "failed_outcomes": event_metrics.iter().filter(|event| model::string(event, "outcome").as_deref() == Some("failed")).count(),
        "decision_outcomes": event_metrics.iter().filter(|event| model::string(event, "outcome").as_deref() == Some("decision")).count(),
        "timeout_outcomes": event_metrics.iter().filter(|event| model::string(event, "outcome").as_deref() == Some("timeout")).count(),
        "operations": operations.len(),
        "accepted_operations": operations.iter().filter(|operation| !operation.get("accepted_at").map(Value::is_null).unwrap_or(true)).count(),
        "stalls": stall_events.len(),
        "stall_reasons": stall_reasons
    })
}

fn open_waits(s: &J, task: &J, now: f64) -> Vec<J> {
    let mut result = Vec::new();
    if let Some(waits) = model::get(s, "waits").and_then(Value::as_object) {
        for wait in waits.values() {
            if model::bool_or(wait, "closed", false)
                || model::get(wait, "task") != model::get(task, "task")
            {
                continue;
            }
            let registered = timestamp(model::get(wait, "registered_at"));
            let deadline = timestamp(model::get(wait, "deadline"));
            let age = span(registered, Some(now), "registered time missing")
                .get("seconds")
                .cloned()
                .unwrap_or(Value::Null);
            result.push(json!({
                "id": wait.get("id").cloned().unwrap_or(Value::Null),
                "kind": wait.get("kind").cloned().unwrap_or(Value::Null),
                "owner": wait.get("owner").cloned().unwrap_or(Value::Null),
                "condition": wait.get("condition").cloned().unwrap_or(Value::Null),
                "registered_at": registered,
                "deadline": deadline,
                "age": age,
                "overdue": deadline.map(|value| now >= value).unwrap_or(false)
            }));
        }
    }
    result
}

fn task_metrics(s: &J, task: &J, deliveries: &BTreeMap<String, Vec<String>>, now: f64) -> J {
    let task_name = model::string(task, "task").unwrap_or_default();
    let events: Vec<J> = model::get(s, "events")
        .and_then(Value::as_object)
        .map(|events| {
            events
                .values()
                .filter(|event| model::string(event, "task").as_deref() == Some(task_name.as_str()))
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    let event_metrics = task_event_metrics(s, task, &events, deliveries);
    let stall_events: Vec<J> = events
        .iter()
        .filter(|event| model::string(event, "kind").as_deref() == Some("progress_stall"))
        .cloned()
        .collect();
    let mut current: Vec<J> = event_metrics
        .iter()
        .filter(|event| model::bool_or(event, "current", false))
        .cloned()
        .collect();
    if current.is_empty() {
        current = event_metrics.clone();
    }
    let applied: Vec<J> = current
        .iter()
        .filter(|event| model::bool_or(event, "applied", false))
        .cloned()
        .collect();
    let first_result = current
        .iter()
        .filter_map(|event| timestamp(model::get(event, "observed_at")))
        .min_by(|left, right| left.partial_cmp(right).unwrap_or(std::cmp::Ordering::Equal));
    let last_applied = applied
        .iter()
        .filter_map(|event| timestamp(model::get(event, "applied_at")))
        .max_by(|left, right| left.partial_cmp(right).unwrap_or(std::cmp::Ordering::Equal));
    let completed = timestamp(model::get(task, "completed_at"));
    let by_wait = event_by_wait(&events);
    let operations: Vec<J> = model::get(task, "operations")
        .and_then(Value::as_object)
        .map(|operations| {
            operations
                .values()
                .map(|operation| operation_metrics(&by_wait, operation))
                .collect()
        })
        .unwrap_or_default();
    let task_segments = task_segments(task, first_result, last_applied, completed, now);
    let counts = task_counts(task, &event_metrics, &applied, &stall_events, &operations);
    let history = model::array_field(task, "history")
        .cloned()
        .unwrap_or_default();
    let interventions = counter(
        history
            .iter()
            .filter_map(|entry| model::string(entry, "action"))
            .filter(|action| {
                matches!(
                    action.as_str(),
                    "recovery_wait" | "control" | "writer_released"
                )
            }),
    );
    json!({
        "task": task.get("task").cloned().unwrap_or(Value::Null),
        "epoch": task.get("epoch").cloned().unwrap_or(Value::Null),
        "assignment": task.get("assignment").cloned().unwrap_or(Value::Null),
        "candidate": task.get("candidate").cloned().unwrap_or(Value::Null),
        "phase": task.get("phase").cloned().unwrap_or(Value::Null),
        "control": task.get("control").cloned().unwrap_or(Value::Null),
        "owner": task.get("owner").cloned().unwrap_or(Value::Null),
        "created_at": timestamp(model::get(task, "created_at")),
        "completed_at": completed,
        "counts": counts,
        "recorded_interventions": interventions,
        "manual_intervention_note": "only persisted recovery/control/release transitions are counted; chat urging is not derivable",
        "segments": task_segments,
        "events": event_metrics,
        "operations": operations,
        "open_waits": open_waits(s, task, now)
    })
}

fn segment_names(tasks: &[J]) -> Vec<String> {
    let mut names = initial_segment_names();
    for task in tasks {
        add_task_segment_names(&mut names, task);
    }
    names
}

fn initial_segment_names() -> Vec<String> {
    vec![
        "created_to_first_result".to_string(),
        "created_to_completed".to_string(),
        "last_applied_to_completed".to_string(),
    ]
}

fn add_task_segment_names(names: &mut Vec<String>, task: &J) {
    add_segment_names(names, model::get(task, "segments"));
    add_segment_list_names(names, model::array_field(task, "events"));
    add_segment_list_names(names, model::array_field(task, "operations"));
}

fn add_segment_names(names: &mut Vec<String>, source: Option<&J>) {
    let Some(segments) = source else {
        return;
    };
    let Some(segments) = segments.as_object() else {
        return;
    };
    for key in segments.keys() {
        add_segment_name(names, key);
    }
}

fn add_segment_list_names(names: &mut Vec<String>, items: Option<&Vec<J>>) {
    let Some(items) = items else {
        return;
    };
    for item in items {
        add_segment_names(names, model::get(item, "segments"));
    }
}

fn add_segment_name(names: &mut Vec<String>, name: &str) {
    if names.iter().any(|existing| existing == name) {
        return;
    }
    names.push(name.to_string());
}

fn segment_values(tasks: &[J], name: &str) -> (Vec<f64>, usize, usize) {
    let mut values = Vec::new();
    let mut unknown = 0usize;
    let mut ongoing = 0usize;
    for task in tasks {
        let mut sources: Vec<&J> = Vec::new();
        if let Some(segments) = model::get(task, "segments") {
            sources.push(segments);
        }
        if let Some(events) = model::array_field(task, "events") {
            sources.extend(
                events
                    .iter()
                    .filter_map(|event| model::get(event, "segments")),
            );
        }
        if let Some(operations) = model::array_field(task, "operations") {
            sources.extend(
                operations
                    .iter()
                    .filter_map(|operation| model::get(operation, "segments")),
            );
        }
        for source in sources {
            let Some(segment) = model::get(source, name) else {
                continue;
            };
            if model::bool_or(segment, "ongoing", false) {
                ongoing += 1;
            } else if segment.get("seconds").map(Value::is_null).unwrap_or(true) {
                unknown += 1;
            } else if let Some(seconds) = model::number(segment, "seconds") {
                values.push(seconds);
            }
        }
    }
    (values, unknown, ongoing)
}

fn segment_summary(values: &[f64], unknown: usize, ongoing: usize) -> J {
    let count = values.len();
    let min = values
        .iter()
        .copied()
        .min_by(|left, right| left.partial_cmp(right).unwrap_or(std::cmp::Ordering::Equal));
    let max = values
        .iter()
        .copied()
        .max_by(|left, right| left.partial_cmp(right).unwrap_or(std::cmp::Ordering::Equal));
    let median = if values.is_empty() {
        None
    } else {
        let mut sorted = values.to_vec();
        sorted.sort_by(|left, right| left.partial_cmp(right).unwrap_or(std::cmp::Ordering::Equal));
        let middle = sorted.len() / 2;
        Some(round3(if sorted.len() % 2 == 0 {
            (sorted[middle - 1] + sorted[middle]) / 2.0
        } else {
            sorted[middle]
        }))
    };
    let mean = if values.is_empty() {
        None
    } else {
        Some(round3(values.iter().sum::<f64>() / values.len() as f64))
    };
    json!({
        "count": count,
        "unknown": unknown,
        "ongoing": ongoing,
        "min": min,
        "median": median,
        "mean": mean,
        "max": max
    })
}

fn aggregate(tasks: &[J]) -> J {
    let mut segments = Map::new();
    for name in segment_names(tasks) {
        let (values, unknown, ongoing) = segment_values(tasks, &name);
        segments.insert(name, segment_summary(&values, unknown, ongoing));
    }
    let mut counts = BTreeMap::new();
    for task in tasks {
        if let Some(task_counts) = model::get(task, "counts").and_then(Value::as_object) {
            for (key, value) in task_counts {
                if key == "stall_reasons" {
                    continue;
                }
                if let Some(number) = value.as_i64() {
                    *counts.entry(key.clone()).or_insert(0i64) += number;
                }
            }
        }
    }
    let counts: Map<String, Value> = counts
        .into_iter()
        .map(|(key, value)| (key, json!(value)))
        .collect();
    json!({"tasks": tasks.len(), "segments": segments, "counts": counts})
}

fn capabilities(root: &Path, s: &J, now: f64) -> J {
    let guard_dir = model::string(s, "guard_dir");
    let mut result = base_capabilities(root, s, guard_dir.clone());
    apply_readiness_capability(&mut result, root, now);
    apply_guard_capability(&mut result, guard_dir);
    result
}

fn base_capabilities(root: &Path, s: &J, guard_dir: Option<String>) -> J {
    json!({
        "notification": {
            "notify": model::bool_or(s, "notify", false),
            "stopped": model::bool_or(s, "stopped", false),
            "delivery_status": s.get("delivery_status").cloned().unwrap_or(Value::Null),
            "queue_watcher": watch_service::health(root)
        },
        "receiver": s.get("receiver").cloned().unwrap_or(Value::Null),
        "guard_dir": guard_dir,
        "context_telemetry": "unknown",
        "auto_rotate": "unknown",
        "guard_phase": "unknown",
        "last_readiness": null,
        "readiness_age_seconds": null
    })
}

fn apply_readiness_capability(result: &mut J, root: &Path, now: f64) {
    let readiness_path = root.join("readiness.json");
    if !readiness_path.exists() {
        return;
    }
    add_readiness_capability(result, util::read_json(&readiness_path), now);
}

fn add_readiness_capability(result: &mut J, readiness: Result<J>, now: f64) {
    let Ok(readiness) = readiness else {
        result["last_readiness"] = json!({"error": "readiness.json is unavailable or malformed"});
        return;
    };
    let checked = timestamp(model::get(&readiness, "checked_at"));
    result["last_readiness"] = json!({
        "active_wakeup": readiness.get("active_wakeup").cloned().unwrap_or(Value::Null),
        "safe_rotation": readiness.get("safe_rotation").cloned().unwrap_or(Value::Null),
        "gaps": readiness.get("gaps").cloned().unwrap_or_else(|| json!([])),
        "next_steps": readiness.get("next_steps").cloned().unwrap_or_else(|| json!([])),
        "checked_at": checked,
        "last_confirmed_receipt": readiness.get("last_confirmed_receipt").cloned().unwrap_or(Value::Null)
    });
    apply_readiness_age(result, checked, now);
}

fn apply_readiness_age(result: &mut J, checked: Option<f64>, now: f64) {
    let Some(checked) = checked else {
        result["readiness_age_seconds"] = Value::Null;
        return;
    };
    apply_readiness_elapsed(result, checked, now);
}

fn apply_readiness_elapsed(result: &mut J, checked: f64, now: f64) {
    if now < checked {
        result["readiness_age_seconds"] = Value::Null;
        result["readiness_clock_anomaly"] = json!("readiness time is in the future");
        return;
    }
    result["readiness_age_seconds"] = json!(round3(now - checked));
}

fn apply_guard_capability(result: &mut J, guard_dir: Option<String>) {
    let Some(guard_dir) = guard_dir else {
        return;
    };
    let path = Path::new(&guard_dir);
    add_guard_capability(result, path, util::read_json(&path.join("state.json")));
}

fn add_guard_capability(result: &mut J, path: &Path, state: Result<J>) {
    let Ok(state) = state else {
        result["guard_state"] = json!("unavailable or malformed");
        return;
    };
    apply_guard_state(result, path, &state);
}

fn apply_guard_state(result: &mut J, path: &Path, state: &J) {
    let usage = model::get(state, "usage").cloned().unwrap_or(Value::Null);
    result["guard_phase"] = state.get("phase").cloned().unwrap_or(Value::Null);
    result["auto_rotate"] = json!(model::bool_or(state, "auto_rotate", false));
    result["context_telemetry"] = json!(context_telemetry(&usage));
    result["guard_epoch"] = state.get("epoch").cloned().unwrap_or(Value::Null);
    result["guard_level"] = state.get("level").cloned().unwrap_or(Value::Null);
    result["guard_watcher"] = watch_service::health(path);
}

fn context_telemetry(usage: &J) -> &'static str {
    let known = usage
        .as_object()
        .map(|value| !value.is_empty())
        .unwrap_or(false);
    if known {
        "known"
    } else {
        "unknown"
    }
}

pub fn report(root: &Path, now: Option<f64>) -> Result<J> {
    let now = now.unwrap_or_else(util::now);
    let s = util::read_json(&root.join("queue.json"))?;
    let (deliveries, duplicates) = delivery_index(&s);
    let mut tasks: Vec<J> = model::get(&s, "tasks")
        .and_then(Value::as_object)
        .map(|tasks| {
            tasks
                .values()
                .map(|task| task_metrics(&s, task, &deliveries, now))
                .collect()
        })
        .unwrap_or_default();
    tasks.sort_by(|left, right| {
        let left_key = (
            model::number(left, "created_at").unwrap_or(0.0),
            model::string(left, "task").unwrap_or_default(),
        );
        let right_key = (
            model::number(right, "created_at").unwrap_or(0.0),
            model::string(right, "task").unwrap_or_default(),
        );
        left_key
            .0
            .partial_cmp(&right_key.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(left_key.1.cmp(&right_key.1))
    });
    Ok(json!({
        "queue": s.get("id").cloned().unwrap_or(Value::Null),
        "as_of": now,
        "stopped": model::bool_or(&s, "stopped", false),
        "notify": model::bool_or(&s, "notify", false),
        "delivery_status": s.get("delivery_status").cloned().unwrap_or(Value::Null),
        "tasks": tasks,
        "aggregate": aggregate(&tasks),
        "duplicate_deliveries": duplicates,
        "capabilities": capabilities(root, &s, now),
        "notes": [
            "missing values are unknown, not zero",
            "ongoing waits are reported separately from completed spans",
            "metrics are read-only and cannot dispatch, apply or rerun work"
        ],
        "limitations": [
            "validation/build resources are not stored in queue.json; use command or CI evidence",
            "chat urging is not derivable; only persisted recovery/control/release transitions are counted",
            "notification transport state is not proof that a result was applied or a task completed"
        ]
    }))
}

fn display(value: Option<&J>) -> String {
    match value {
        None | Some(Value::Null) => "None".to_string(),
        Some(Value::Bool(value)) => if *value { "True" } else { "False" }.to_string(),
        Some(Value::String(text)) => text.clone(),
        Some(other) => other.to_string(),
    }
}

fn format_span(span: Option<&J>) -> String {
    match span
        .and_then(|span| span.get("seconds"))
        .and_then(Value::as_f64)
    {
        Some(value) => format!("{value:.3}s"),
        None => "unknown".to_string(),
    }
}

fn segment(task: &J, name: &str) -> String {
    format_span(model::dig(task, &["segments", name]))
}

pub fn format_report(result: &J) -> String {
    let mut lines = vec![format_report_header(result)];
    append_task_report_lines(&mut lines, result);
    append_aggregate_report_lines(&mut lines, result);
    append_capability_report_line(&mut lines, result);
    append_report_tail(&mut lines, result);
    lines.join("\n")
}

fn format_report_header(result: &J) -> String {
    format!(
        "queue={} | tasks={} | stopped={} | notify={} | delivery={}",
        model::string(result, "queue").unwrap_or_default(),
        model::array_field(result, "tasks")
            .map(Vec::len)
            .unwrap_or(0),
        display(model::get(result, "stopped")),
        display(model::get(result, "notify")),
        display(model::get(result, "delivery_status"))
    )
}

fn append_task_report_lines(lines: &mut Vec<String>, result: &J) {
    let Some(tasks) = model::array_field(result, "tasks") else {
        return;
    };
    for task in tasks {
        append_task_report_line(lines, task);
    }
}

fn append_task_report_line(lines: &mut Vec<String>, task: &J) {
    let counts = model::get(task, "counts")
        .cloned()
        .unwrap_or_else(|| json!({}));
    lines.push(format_task_summary_line(task, &counts));
    lines.push(format_task_segment_line(task));
    append_event_report_line(lines, task);
    append_operation_report_line(lines, task);
    append_open_waits_report_line(lines, task);
    append_stall_reasons_report_line(lines, &counts);
}

fn format_task_summary_line(task: &J, counts: &J) -> String {
    format!(
        "{} | phase={} | control={} | events={} applied={} stalls={}",
        model::string(task, "task").unwrap_or_default(),
        model::string(task, "phase").unwrap_or_default(),
        model::string(task, "control").unwrap_or_default(),
        model::int(counts, "events").unwrap_or(0),
        model::int(counts, "applied_events").unwrap_or(0),
        model::int(counts, "stalls").unwrap_or(0)
    )
}

fn format_task_segment_line(task: &J) -> String {
    format!(
        "  created->result={} created->complete={}",
        segment(task, "created_to_first_result"),
        segment(task, "created_to_completed")
    )
}

fn append_event_report_line(lines: &mut Vec<String>, task: &J) {
    let Some(event) = model::array_field(task, "events").and_then(|events| events.first()) else {
        return;
    };
    lines.push(format!(
        "  ready->delivery={} delivery->ACK={} ACK->applied={}",
        format_span(model::dig(event, &["segments", "ready_to_delivery"])),
        format_span(model::dig(event, &["segments", "delivery_to_ack"])),
        format_span(model::dig(event, &["segments", "ack_to_applied"]))
    ));
}

fn append_operation_report_line(lines: &mut Vec<String>, task: &J) {
    let Some(operation) =
        model::array_field(task, "operations").and_then(|operations| operations.first())
    else {
        return;
    };
    lines.push(format!(
        "  prepared->started={} prepared->accepted={} accepted->next-result={}",
        format_span(model::dig(operation, &["segments", "prepared_to_started"])),
        format_span(model::dig(operation, &["segments", "prepared_to_accepted"])),
        format_span(model::dig(
            operation,
            &["segments", "accepted_to_next_result"]
        ))
    ));
}

fn append_open_waits_report_line(lines: &mut Vec<String>, task: &J) {
    let Some(waits) = model::array_field(task, "open_waits") else {
        return;
    };
    if waits.is_empty() {
        return;
    }
    let names = waits
        .iter()
        .map(format_open_wait)
        .collect::<Vec<_>>()
        .join(", ");
    lines.push(format!("  open-waits={names}"));
}

fn format_open_wait(wait: &J) -> String {
    let status = if model::bool_or(wait, "overdue", false) {
        "overdue"
    } else {
        "waiting"
    };
    format!(
        "{}({status})",
        model::string(wait, "id").unwrap_or_default()
    )
}

fn append_stall_reasons_report_line(lines: &mut Vec<String>, counts: &J) {
    let Some(reasons) = model::dig(counts, &["stall_reasons"]).and_then(Value::as_object) else {
        return;
    };
    if reasons.is_empty() {
        return;
    }
    let text = reasons
        .iter()
        .map(|(key, value)| format!("{key}:{}", value.as_i64().unwrap_or(0)))
        .collect::<Vec<_>>()
        .join(", ");
    lines.push(format!("  stall-reasons={text}"));
}

fn append_aggregate_report_lines(lines: &mut Vec<String>, result: &J) {
    lines.push("aggregate:".to_string());
    let aggregate = model::dig(result, &["aggregate", "segments"])
        .cloned()
        .unwrap_or_else(|| json!({}));
    for name in [
        "ready_to_delivery",
        "delivery_to_ack",
        "ack_to_applied",
        "prepared_to_started",
        "prepared_to_accepted",
        "accepted_to_next_result",
        "created_to_completed",
    ] {
        lines.push(format_aggregate_report_line(&aggregate, name));
    }
}

fn format_aggregate_report_line(aggregate: &J, name: &str) -> String {
    let stat = model::get(aggregate, name)
        .cloned()
        .unwrap_or_else(|| json!({}));
    format!(
        "  {name}: n={} unknown={} median={} mean={} max={}",
        model::int(&stat, "count").unwrap_or(0),
        model::int(&stat, "unknown").unwrap_or(0),
        display(model::get(&stat, "median")),
        display(model::get(&stat, "mean")),
        display(model::get(&stat, "max"))
    )
}

fn append_capability_report_line(lines: &mut Vec<String>, result: &J) {
    let capabilities = model::get(result, "capabilities")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let notification = model::get(&capabilities, "notification")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let receiver = model::get(&capabilities, "receiver")
        .cloned()
        .unwrap_or_else(|| json!({}));
    lines.push(format!(
        "capabilities: notify={} delivery={} telemetry={} auto_rotate={} receiver={}/{}",
        model::bool_or(&notification, "notify", false),
        model::string(&notification, "delivery_status").unwrap_or_default(),
        display(model::get(&capabilities, "context_telemetry")),
        display(model::get(&capabilities, "auto_rotate")),
        model::string(&receiver, "pane").unwrap_or_else(|| "unknown".to_string()),
        model::string(&receiver, "task").unwrap_or_else(|| "unknown".to_string())
    ));
    append_readiness_report_line(lines, &capabilities);
}

fn append_readiness_report_line(lines: &mut Vec<String>, capabilities: &J) {
    let Some(readiness) =
        model::get(capabilities, "last_readiness").filter(|value| !value.is_null())
    else {
        return;
    };
    lines.push(format!(
        "readiness: active_wakeup={} safe_rotation={} age={}s",
        display(model::get(readiness, "active_wakeup")),
        display(model::get(readiness, "safe_rotation")),
        display(model::get(capabilities, "readiness_age_seconds"))
    ));
}

fn append_report_tail(lines: &mut Vec<String>, result: &J) {
    append_duplicate_report_line(lines, result);
    append_limitation_report_lines(lines, result);
}

fn append_duplicate_report_line(lines: &mut Vec<String>, result: &J) {
    let Some(duplicates) = model::array_field(result, "duplicate_deliveries") else {
        return;
    };
    if duplicates.is_empty() {
        return;
    }
    lines.push(format!(
        "duplicate-delivery-associations={}",
        duplicates.len()
    ));
}

fn append_limitation_report_lines(lines: &mut Vec<String>, result: &J) {
    let Some(limitations) = model::array_field(result, "limitations") else {
        return;
    };
    for limitation in limitations {
        append_limitation_report_line(lines, limitation);
    }
}

fn append_limitation_report_line(lines: &mut Vec<String>, limitation: &J) {
    if let Some(text) = limitation.as_str() {
        lines.push(format!("limitation: {text}"));
    }
}
