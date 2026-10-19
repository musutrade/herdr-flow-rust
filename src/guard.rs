//! Cooperative Codex context monitoring and acknowledged Herdr session rotation.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

use crate::error::{ErrorKind, FlowError, Result};
use crate::herdr::{GuardAdapter, Herdr};
use crate::logging;
use crate::model::{self, J};
use crate::util;
use crate::util::process::command;
use crate::util::pyjson::{digest, python_sorted_compact_utf8, sha256_hex, uuid_hex};
use crate::watch_service;

pub const ROTATION_PHASES: &[&str] = &[
    "tui_exit_sending",
    "awaiting_shell",
    "tui_launch_sending",
    "awaiting_tui",
    "runtime_restore_sending",
    "successor_ready",
    "resume_sending",
    "awaiting_ack",
];

pub const OBSERVATION_PHASES: &[&str] = &[
    "monitoring",
    "requested",
    "ready",
    "parked",
    "awaiting_shell",
    "awaiting_tui",
    "successor_ready",
    "awaiting_ack",
];

pub const TRANSIENT_LIMIT: u64 = 3;
pub const HEAD_BYTES: usize = 4096;
pub const STDERR_LIMIT: usize = 2048;

pub fn guard_error(message: impl Into<String>) -> FlowError {
    FlowError::guard(message)
}

pub fn scope(g: &J) -> J {
    json!({
        "task": g.get("task").cloned().unwrap_or(Value::Null),
        "role": g.get("role").cloned().unwrap_or(Value::Null),
        "pane": g.get("pane").cloned().unwrap_or(Value::Null),
        "terminal": g.get("terminal").cloned().unwrap_or(Value::Null),
        "socket": g.get("socket").cloned().unwrap_or(Value::Null),
        "cwd": g.get("cwd").cloned().unwrap_or(Value::Null)
    })
}

pub fn locked(root: &Path) -> Result<util::FileLock> {
    let root = util::resolve_path(root);
    std::fs::create_dir_all(&root)?;
    match util::FileLock::acquire(&root.join("guard.lock"), false)? {
        Some(lock) => Ok(lock),
        None => Err(FlowError::new(
            ErrorKind::StateBusy,
            "guard state is busy; retry after this poll",
        )),
    }
}

pub fn save(root: &Path, s: &mut J) -> Result<()> {
    s["updated_at"] = json!(util::now());
    util::atomic_write_json(&util::resolve_path(root).join("state.json"), s)
}

fn read_state(root: &Path) -> Result<J> {
    util::read_json(&util::resolve_path(root).join("state.json"))
}

pub fn review_events(root: &Path, s: &J) -> Result<J> {
    let path = util::resolve_path(root).join("review-events.json");
    if !path.exists() {
        return Ok(review_events_absent());
    }
    review_events_read(&path, s)
}

fn review_events_absent() -> J {
    json!({"status": "not_observed", "events": []})
}

fn review_events_stale() -> J {
    json!({"status": "stale", "events": []})
}

fn review_events_read(path: &Path, s: &J) -> Result<J> {
    let journal = util::read_json(path)?;
    review_events_checked(journal, s)
}

fn review_events_checked(journal: J, s: &J) -> Result<J> {
    if !review_events_valid(&journal) {
        return Err(guard_error("invalid automatic review event snapshot"));
    }
    review_events_identity(journal, s)
}

fn review_events_valid(journal: &J) -> bool {
    [
        model::get(journal, "version").and_then(Value::as_i64) == Some(1),
        model::get(journal, "events")
            .and_then(Value::as_array)
            .is_some(),
        model::get(journal, "overflow_count")
            .and_then(Value::as_i64)
            .is_some(),
    ]
    .into_iter()
    .all(|valid| valid)
}

fn review_events_identity(journal: J, s: &J) -> Result<J> {
    if review_events_is_stale(&journal, s) {
        return Ok(review_events_stale());
    }
    Ok(review_events_observed(journal))
}

fn review_events_is_stale(journal: &J, s: &J) -> bool {
    let expected_thread = review_events_expected_thread(s);
    [
        model::get(s, "bridge_nonce").is_none(),
        model::string(journal, "nonce") != model::string(s, "bridge_nonce"),
        model::get(journal, "thread_id") != Some(&expected_thread),
    ]
    .into_iter()
    .any(|stale| stale)
}

fn review_events_expected_thread(s: &J) -> J {
    model::get(s, "successor")
        .cloned()
        .or_else(|| model::get(s, "session").cloned())
        .unwrap_or(Value::Null)
}

fn review_events_observed(mut journal: J) -> J {
    journal["status"] = json!("observed");
    journal
}

fn review_state(root: &Path) -> Result<(J, J)> {
    let s = read_state(root)?;
    review_state_checked(s)
}

fn review_state_checked(s: J) -> Result<(J, J)> {
    if review_state_disallowed(&s) {
        return Err(guard_error(
            "disable a settled guard and acknowledge deliveries before configuring review",
        ));
    }
    review_state_transcript(s)
}

fn review_state_disallowed(s: &J) -> bool {
    [
        model::int(s, "version") != Some(2),
        !matches!(
            model::string(s, "phase").as_deref(),
            Some("monitoring" | "parked")
        ),
        model::bool_or(s, "auto_rotate", false),
        external_delivery_pending(s),
    ]
    .into_iter()
    .any(|disallowed| disallowed)
}

fn review_state_transcript(s: J) -> Result<(J, J)> {
    let transcript = model::string(&s, "transcript")
        .ok_or_else(|| guard_error("review configuration transcript identity mismatch"))?;
    review_state_data(s, &transcript)
}

fn review_state_data(s: J, transcript: &str) -> Result<(J, J)> {
    let data = transcript_state(Path::new(transcript), None)?;
    review_state_identity(s, data)
}

fn review_state_identity(s: J, data: J) -> Result<(J, J)> {
    if review_state_identity_mismatch(&s, &data) {
        return Err(guard_error(
            "review configuration transcript identity mismatch",
        ));
    }
    Ok((s, data))
}

fn review_state_identity_mismatch(s: &J, data: &J) -> bool {
    [
        model::string(data, "session") != model::string(s, "session"),
        model::string(data, "cwd") != model::string(s, "cwd"),
    ]
    .into_iter()
    .any(|mismatch| mismatch)
}

fn external_delivery_pending(s: &J) -> bool {
    model::get(s, "external_delivery")
        .map(|value| model::truthy(Some(value)))
        .unwrap_or(false)
}

fn review_attestation(s: &J, handoff: Option<&Path>, data: &J) -> Result<()> {
    if model::int(data, "user_messages").unwrap_or(0) == 0 {
        return Ok(());
    }
    let Some(handoff) = handoff else {
        return Err(guard_error(
            "an existing work session requires a quiescent --handoff attestation",
        ));
    };
    verify_handoff(s, handoff)?;
    Ok(())
}

fn review_adapter(h: &mut dyn GuardAdapter, require_bridge: bool) -> Result<J> {
    if require_bridge {
        return review_adapter_bridge(h);
    }
    review_adapter_idle(h)
}

fn review_adapter_bridge(h: &mut dyn GuardAdapter) -> Result<J> {
    let marker = h.marker()?;
    review_adapter_marker(h, marker)
}

fn review_adapter_marker(h: &mut dyn GuardAdapter, marker: Option<J>) -> Result<J> {
    if !review_adapter_ready(marker.as_ref()) {
        return Err(guard_error(
            "reattach the quiescent native TUI through the updated bridge before enabling review",
        ));
    }
    review_adapter_idle(h)
}

fn review_adapter_ready(marker: Option<&J>) -> bool {
    marker.and_then(|marker| model::int(marker, "review_events_version")) == Some(1)
}

fn review_adapter_idle(h: &mut dyn GuardAdapter) -> Result<J> {
    let idle = h.idle()?;
    review_adapter_idle_check(h, idle)
}

fn review_adapter_idle_check(h: &mut dyn GuardAdapter, idle: bool) -> Result<J> {
    if !idle {
        return Err(guard_error(
            "review configuration requires an idle registered native thread",
        ));
    }
    h.snapshot(None)
}

fn apply_review(root: &Path, s: &mut J, h: &mut dyn GuardAdapter, before: &J) -> Result<J> {
    let expected = crate::herdr::auto_review_runtime(before)?;
    let checkout = fingerprint(&model::string(s, "cwd").unwrap_or_default())?;
    let change = json!({
        "session": s.get("session").cloned().unwrap_or(Value::Null),
        "before": before,
        "expected": expected,
        "previous_phase": s.get("phase").cloned().unwrap_or(Value::Null),
        "checkout": checkout,
        "at": util::now()
    });
    s["review_change"] = change.clone();
    s["phase"] = json!("review_config_sending");
    save(root, s)?;
    h.set_state(s);
    let result = (|| -> Result<J> {
        h.rpc(
            "thread/settings/update",
            &json!({
                "threadId": s.get("session").cloned().unwrap_or(Value::Null),
                "approvalPolicy": "on-request",
                "approvalsReviewer": "auto_review",
                "sandboxPolicy": expected.get("sandbox").cloned().unwrap_or(Value::Null)
            }),
        )?;
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let actual = h.snapshot(None)?;
            match crate::herdr::verify_review_runtime(&expected, &actual) {
                Ok(()) => break,
                Err(error) => {
                    if Instant::now() >= deadline {
                        return Err(error);
                    }
                    thread::sleep(Duration::from_millis(100));
                }
            }
        }
        let actual = h.snapshot(None)?;
        if fingerprint(&model::string(s, "cwd").unwrap_or_default())?
            != change["checkout"].as_str().unwrap_or_default()
        {
            return Err(guard_error("checkout changed during review configuration"));
        }
        Ok(actual)
    })();
    match result {
        Ok(actual) => {
            s["phase"] = change
                .get("previous_phase")
                .cloned()
                .unwrap_or_else(|| json!("monitoring"));
            s["review_policy"] = actual;
            s["review_change"]["status"] = json!("verified");
            save(root, s)?;
            Ok(s.clone())
        }
        Err(error) => {
            s["phase"] = json!("needs_attention");
            s["phase_before_error"] = json!("review_config_sending");
            s["error"] = json!(error.to_string());
            save(root, s)?;
            Err(guard_error(
                "review configuration unconfirmed; inspect then review-reconcile, never replay",
            ))
        }
    }
}

pub fn configure_review(
    root: &Path,
    handoff: Option<&Path>,
    h: &mut dyn GuardAdapter,
    require_bridge: bool,
) -> Result<J> {
    let (mut s, data) = review_state(root)?;
    review_attestation(&s, handoff, &data)?;
    h.set_state(&s);
    let before = review_adapter(h, require_bridge)?;
    apply_review(root, &mut s, h, &before)
}

pub fn reconcile_review(root: &Path, h: &mut dyn GuardAdapter) -> Result<J> {
    let s = read_state(root)?;
    reconcile_review_state(root, s, h)
}

fn reconcile_review_state(root: &Path, s: J, h: &mut dyn GuardAdapter) -> Result<J> {
    reconcile_review_phase(&s)?;
    reconcile_review_live(root, s, h)
}

fn reconcile_review_phase(s: &J) -> Result<()> {
    if reconcile_review_phase_ready(s) {
        return Ok(());
    }
    Err(guard_error(
        "no ambiguous review configuration to reconcile",
    ))
}

fn reconcile_review_phase_ready(s: &J) -> bool {
    let phase = model::string(s, "phase").unwrap_or_default();
    let before = model::string(s, "phase_before_error").unwrap_or_default();
    [phase].into_iter().any(|phase| {
        phase == "review_config_sending"
            || (phase == "needs_attention" && before == "review_config_sending")
    })
}

fn reconcile_review_live(root: &Path, s: J, h: &mut dyn GuardAdapter) -> Result<J> {
    let change = reconcile_review_change(&s)?;
    reconcile_review_identity(root, s, h, change)
}

fn reconcile_review_change(s: &J) -> Result<J> {
    model::get(s, "review_change")
        .cloned()
        .ok_or_else(|| guard_error("review configuration journal is unavailable"))
}

fn reconcile_review_identity(root: &Path, s: J, h: &mut dyn GuardAdapter, change: J) -> Result<J> {
    if model::get(&s, "session") != model::get(&change, "session") {
        return Err(guard_error(
            "review configuration session or checkout changed",
        ));
    }
    reconcile_review_checkout(root, s, h, change)
}

fn reconcile_review_checkout(root: &Path, s: J, h: &mut dyn GuardAdapter, change: J) -> Result<J> {
    let checkout = reconcile_review_checkout_value(&s)?;
    reconcile_review_checkout_compare(root, s, h, change, checkout)
}

fn reconcile_review_checkout_compare(
    root: &Path,
    s: J,
    h: &mut dyn GuardAdapter,
    change: J,
    checkout: String,
) -> Result<J> {
    if checkout != model::string(&change, "checkout").unwrap_or_default() {
        return Err(guard_error(
            "review configuration session or checkout changed",
        ));
    }
    reconcile_review_apply(root, s, h, change)
}

fn reconcile_review_checkout_value(s: &J) -> Result<String> {
    fingerprint(&model::string(s, "cwd").unwrap_or_default())
}

fn reconcile_review_apply(root: &Path, mut s: J, h: &mut dyn GuardAdapter, change: J) -> Result<J> {
    h.set_state(&s);
    let actual = reconcile_review_actual(h)?;
    reconcile_review_stored(root, &mut s, &change, &actual)
}

fn reconcile_review_stored(root: &Path, s: &mut J, change: &J, actual: &J) -> Result<J> {
    let status = reconcile_review_status(s, change, actual)?;
    reconcile_review_persist(root, s, change, status)
}

fn reconcile_review_actual(h: &mut dyn GuardAdapter) -> Result<J> {
    let idle = h.idle()?;
    reconcile_review_actual_check(h, idle)
}

fn reconcile_review_actual_check(h: &mut dyn GuardAdapter, idle: bool) -> Result<J> {
    if !idle {
        return Err(guard_error(
            "inspect after the registered thread becomes idle",
        ));
    }
    h.snapshot(None)
}

fn reconcile_review_status(s: &mut J, change: &J, actual: &J) -> Result<&'static str> {
    crate::herdr::verify_review_runtime(change.get("expected").unwrap_or(&Value::Null), actual)
        .map(|()| {
            s["review_policy"] = actual.clone();
            "verified"
        })
        .or_else(|error| reconcile_review_not_applied(change, actual, error))
}

fn reconcile_review_not_applied(change: &J, actual: &J, error: FlowError) -> Result<&'static str> {
    let before = change.get("before").cloned().unwrap_or_else(|| json!({}));
    reconcile_review_previous(&before, actual, &error)?;
    Ok("not_applied")
}

fn reconcile_review_previous(before: &J, actual: &J, error: &FlowError) -> Result<()> {
    crate::herdr::same_runtime(before, actual).map_err(|_| {
        guard_error(format!(
            "review configuration differs from both expected and previous settings: {error}"
        ))
    })
}

fn reconcile_review_persist(root: &Path, s: &mut J, change: &J, status: &str) -> Result<J> {
    reconcile_review_history(root, s)?;
    reconcile_review_finish(root, s, change, status)
}

fn reconcile_review_history(root: &Path, s: &J) -> Result<()> {
    let history = util::resolve_path(root)
        .join("history")
        .join(format!("review-recovery-{}.json", util::now_ns()));
    util::atomic_write_json(&history, s)
}

fn reconcile_review_finish(root: &Path, s: &mut J, change: &J, status: &str) -> Result<J> {
    s["review_change"]["status"] = json!(status);
    s["phase"] = change
        .get("previous_phase")
        .cloned()
        .unwrap_or_else(|| json!("monitoring"));
    s["auto_rotate"] = json!(false);
    model::remove(s, "error");
    model::remove(s, "phase_before_error");
    save(root, s)?;
    Ok(s.clone())
}

fn scan_row(row: &J, state: &mut J) -> Result<()> {
    let payload = model::get(row, "payload")
        .cloned()
        .unwrap_or_else(|| json!({}));
    match model::string(row, "type").as_deref() {
        Some("session_meta") => {
            state["meta"] = json!({
                "id": payload.get("id").cloned().unwrap_or(Value::Null),
                "cwd": payload.get("cwd").cloned().unwrap_or(Value::Null)
            });
        }
        Some("compacted") => {
            state["compacted"] = json!(true);
            state["usage"] = Value::Null;
        }
        Some("event_msg")
            if model::string(&payload, "type").as_deref() == Some("token_count")
                && model::get(&payload, "info").is_some() =>
        {
            if let Some(usage) = token_usage(&payload["info"]) {
                state["usage"] = usage;
            }
        }
        Some("response_item")
            if model::string(&payload, "type").as_deref() == Some("message")
                && model::string(&payload, "role").as_deref() == Some("user") =>
        {
            let users = model::u64_field(state, "users").unwrap_or(0) + 1;
            state["users"] = json!(users);
        }
        _ => {}
    }
    Ok(())
}

fn token_usage(info: &J) -> Option<J> {
    let used = model::dig(info, &["last_token_usage", "total_tokens"]).and_then(Value::as_i64)?;
    let window = model::get(info, "model_context_window").and_then(Value::as_i64)?;
    if used < 0 || window <= 0 {
        return None;
    }
    Some(json!({
        "tokens": used,
        "window": window,
        "ratio": used as f64 / window as f64
    }))
}

fn digest_head(bytes: &[u8], length: usize) -> String {
    sha256_hex(&bytes[..length.min(bytes.len())])
}

/// Read the local rollout format, treating partial writes as incomplete.
pub fn transcript(path: &Path) -> Result<J> {
    transcript_state(path, None)
}

#[derive(Clone, Copy)]
struct TranscriptFacts {
    size: usize,
    dev: u64,
    ino: u64,
}

pub fn transcript_state(path: &Path, cursor: Option<&J>) -> Result<J> {
    let data = util::read_bytes(path)?;
    transcript_state_data(path, cursor, data)
}

fn transcript_state_data(path: &Path, cursor: Option<&J>, data: Vec<u8>) -> Result<J> {
    let metadata = std::fs::metadata(path)?;
    transcript_state_cursor(cursor, data, &metadata)
}

fn transcript_state_cursor(
    cursor: Option<&J>,
    data: Vec<u8>,
    metadata: &std::fs::Metadata,
) -> Result<J> {
    let facts = transcript_facts(metadata);
    let has_object = cursor.and_then(Value::as_object).is_some();
    let cursor = transcript_cursor_map(cursor);
    let base_reusable = transcript_reusable_base(has_object, &cursor, facts);
    let (initial_head_len, initial_head_sha) =
        transcript_head_initial(&cursor, &data, facts, base_reusable);
    let reusable = transcript_head_reusable(base_reusable, &cursor, &initial_head_sha);
    let (head_len, head_sha) =
        transcript_head_final(reusable, &data, facts, initial_head_len, initial_head_sha);
    let state = transcript_initial_state(&cursor, reusable);
    let offset = transcript_initial_offset(&cursor, reusable);
    transcript_state_parse(data, state, offset, facts, head_len, head_sha)
}

fn transcript_facts(metadata: &std::fs::Metadata) -> TranscriptFacts {
    use std::os::unix::fs::MetadataExt;
    TranscriptFacts {
        size: metadata.size() as usize,
        dev: metadata.dev(),
        ino: metadata.ino(),
    }
}

fn transcript_cursor_map(cursor: Option<&J>) -> Map<String, Value> {
    cursor
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default()
}

fn transcript_inode_matches(cursor: &Map<String, Value>, facts: TranscriptFacts) -> bool {
    cursor
        .get("inode")
        .and_then(Value::as_array)
        .map(|inode| {
            inode.len() == 2
                && inode[0].as_u64() == Some(facts.dev)
                && inode[1].as_u64() == Some(facts.ino)
        })
        .unwrap_or(false)
}

fn transcript_reusable_base(
    has_object: bool,
    cursor: &Map<String, Value>,
    facts: TranscriptFacts,
) -> bool {
    let inode_ok = transcript_inode_matches(cursor, facts);
    let offset_ok = facts.size >= transcript_initial_offset(cursor, true);
    let head_ok = cursor.get("head").and_then(Value::as_str).is_some();
    let head_len_ok = cursor
        .get("head_len")
        .and_then(Value::as_u64)
        .map(|length| length as usize <= facts.size)
        .unwrap_or(false);
    [has_object, inode_ok, offset_ok, head_ok, head_len_ok]
        .into_iter()
        .all(|ok| ok)
}

fn transcript_head_initial(
    cursor: &Map<String, Value>,
    data: &[u8],
    facts: TranscriptFacts,
    base_reusable: bool,
) -> (usize, String) {
    let head_len = if base_reusable {
        cursor.get("head_len").and_then(Value::as_u64).unwrap_or(0) as usize
    } else {
        HEAD_BYTES.min(facts.size)
    };
    let head_sha = digest_head(data, head_len);
    (head_len, head_sha)
}

fn transcript_head_reusable(
    base_reusable: bool,
    cursor: &Map<String, Value>,
    head_sha: &str,
) -> bool {
    if !base_reusable {
        return false;
    }
    cursor.get("head").and_then(Value::as_str) == Some(head_sha)
}

fn transcript_head_final(
    reusable: bool,
    data: &[u8],
    facts: TranscriptFacts,
    initial_head_len: usize,
    initial_head_sha: String,
) -> (usize, String) {
    if reusable {
        return (initial_head_len, initial_head_sha);
    }
    let head_len = HEAD_BYTES.min(facts.size);
    let head_sha = digest_head(data, head_len);
    (head_len, head_sha)
}

fn transcript_initial_state(cursor: &Map<String, Value>, reusable: bool) -> J {
    if reusable {
        json!({
            "meta": cursor.get("meta").cloned().unwrap_or(Value::Null),
            "usage": cursor.get("usage").cloned().unwrap_or(Value::Null),
            "users": cursor.get("user_messages").cloned().unwrap_or_else(|| json!(0)),
            "compacted": cursor.get("compacted").cloned().unwrap_or(json!(false))
        })
    } else {
        json!({"meta": null, "usage": null, "users": 0, "compacted": false})
    }
}

fn transcript_initial_offset(cursor: &Map<String, Value>, reusable: bool) -> usize {
    if reusable {
        cursor.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize
    } else {
        0
    }
}

fn transcript_state_parse(
    data: Vec<u8>,
    mut state: J,
    offset: usize,
    facts: TranscriptFacts,
    head_len: usize,
    head_sha: String,
) -> Result<J> {
    let offset = transcript_consume(&data, offset, &mut state)?;
    transcript_state_output(&state, offset, facts, head_len, head_sha)
}

fn transcript_consume(data: &[u8], start: usize, state: &mut J) -> Result<usize> {
    let mut offset = start;
    data[start..]
        .split_inclusive(|byte| *byte == b'\n')
        .take_while(|line| line.ends_with(b"\n"))
        .try_for_each(|line| {
            transcript_scan_line(line, state)?;
            offset += line.len();
            Ok::<(), FlowError>(())
        })?;
    Ok(offset)
}

fn transcript_scan_line(line: &[u8], state: &mut J) -> Result<()> {
    let row: J = serde_json::from_slice(line)
        .map_err(|_| guard_error("malformed complete transcript record"))?;
    scan_row(&row, state)
}

fn transcript_state_output(
    state: &J,
    offset: usize,
    facts: TranscriptFacts,
    head_len: usize,
    head_sha: String,
) -> Result<J> {
    let (id, cwd) = transcript_identity(state)?;
    Ok(transcript_output(
        state, id, cwd, offset, facts, head_len, head_sha,
    ))
}

fn transcript_identity(state: &J) -> Result<(String, String)> {
    let meta = model::get(state, "meta").cloned().unwrap_or(Value::Null);
    let id = model::string(&meta, "id").unwrap_or_default();
    let cwd = model::string(&meta, "cwd").unwrap_or_default();
    if [id.as_str(), cwd.as_str()]
        .into_iter()
        .any(|value| value.is_empty())
    {
        return Err(guard_error(
            "unsupported transcript: session metadata missing",
        ));
    }
    Ok((id, cwd))
}

fn transcript_output(
    state: &J,
    id: String,
    cwd: String,
    offset: usize,
    facts: TranscriptFacts,
    head_len: usize,
    head_sha: String,
) -> J {
    let cwd = util::resolve_path(Path::new(&cwd)).display().to_string();
    let users = model::u64_field(state, "users").unwrap_or(0);
    let usage = state.get("usage").cloned().unwrap_or(Value::Null);
    let compacted = model::bool_or(state, "compacted", false);
    json!({
        "session": id.clone(),
        "cwd": cwd.clone(),
        "usage": usage.clone(),
        "user_messages": users,
        "compacted": compacted,
        "caught_up": offset == facts.size,
        "pending_bytes": facts.size.saturating_sub(offset),
        "cursor": {
            "inode": [facts.dev, facts.ino],
            "offset": offset,
            "head": head_sha,
            "head_len": head_len,
            "meta": {"id": id, "cwd": cwd},
            "usage": usage,
            "user_messages": users,
            "compacted": compacted
        }
    })
}

pub fn find_transcript(session: &str, sessions_dir: &Path) -> Result<PathBuf> {
    let matches = util::files_with_suffix(sessions_dir, &format!("{session}.jsonl"));
    if matches.len() != 1 {
        return Err(guard_error(
            "new session transcript is not uniquely available yet",
        ));
    }
    Ok(util::resolve_path(&matches[0]))
}

pub fn checkout_root(cwd: &Path) -> PathBuf {
    util::checkout_root(cwd)
}

pub fn inside_checkout(path: &Path, cwd: &Path) -> bool {
    util::inside_checkout(path, cwd)
}

pub fn fingerprint(cwd: &str) -> Result<String> {
    util::fingerprint(Path::new(cwd))
}

fn handoff_identity(s: &J, handoff: &J) -> Result<()> {
    for key in ["task", "epoch", "cwd"] {
        if model::get(handoff, key) != model::get(s, key) {
            return Err(guard_error(format!("handoff {key} mismatch")));
        }
    }
    for key in [
        "goal",
        "next_action",
        "authorization",
        "done_when",
        "completed",
        "blockers",
        "preserve",
    ] {
        if model::get(handoff, key).is_none() {
            return Err(guard_error(format!("handoff missing {key}")));
        }
    }
    Ok(())
}

fn handoff_assignment(handoff: &J) -> Result<()> {
    let Some(assignment) = model::get(handoff, "assignment").filter(|value| !value.is_null())
    else {
        return Ok(());
    };
    let valid = assignment.is_object()
        && model::string(assignment, "id")
            .map(|id| !id.trim().is_empty())
            .unwrap_or(false)
        && matches!(
            model::string(assignment, "status").as_deref(),
            Some("pending" | "completed")
        );
    if !valid {
        return Err(guard_error(
            "assignment requires id and pending/completed status",
        ));
    }
    if model::string(assignment, "status").as_deref() == Some("completed") {
        let evidence = model::string(assignment, "evidence").unwrap_or_default();
        if evidence.is_empty()
            || !Path::new(&evidence).is_absolute()
            || !Path::new(&evidence).is_file()
        {
            return Err(guard_error(
                "completed assignment requires existing absolute evidence file",
            ));
        }
    }
    Ok(())
}

fn handoff_processes(handoff: &J) -> Result<()> {
    if !handoff_processes_released(handoff) {
        return Err(guard_error("writer/commands not released"));
    }
    handoff_processes_validate(handoff)
}

fn handoff_processes_released(handoff: &J) -> bool {
    let writer_released = model::get(handoff, "writer")
        .map(Value::is_null)
        .unwrap_or(true);
    let quiescent = model::bool_or(handoff, "quiescent", false);
    let running_commands = model::get(handoff, "running_commands")
        .and_then(Value::as_array)
        .map(Vec::is_empty)
        .unwrap_or(false);
    [writer_released, quiescent, running_commands]
        .into_iter()
        .all(|released| released)
}

fn handoff_processes_validate(handoff: &J) -> Result<()> {
    model::array_field(handoff, "processes")
        .map(|processes| processes.iter().all(handoff_process_ok))
        .unwrap_or(true)
        .then_some(())
        .ok_or_else(|| guard_error("background process has no independent owner/identity"))
}

fn handoff_process_ok(process: &J) -> bool {
    [
        model::bool_or(process, "independent", false),
        model::truthy(model::get(process, "owner")),
        model::truthy(model::get(process, "identity")),
    ]
    .into_iter()
    .all(|independent| independent)
}

pub fn verify_handoff(s: &J, path: &Path) -> Result<Vec<u8>> {
    let raw = util::read_bytes(path)?;
    if raw.len() > 16000 {
        return Err(guard_error(
            "handoff exceeds 16 KB; move history to references",
        ));
    }
    let handoff: J = serde_json::from_slice(&raw)?;
    if !handoff.is_object() {
        return Err(guard_error("handoff must be a JSON object"));
    }
    handoff_identity(s, &handoff)?;
    handoff_assignment(&handoff)?;
    handoff_processes(&handoff)?;
    Ok(raw)
}

pub fn handoff_digest(handoff: &J) -> String {
    digest(python_sorted_compact_utf8(handoff).as_bytes())
}

pub fn advise(root: &Path, path: &Path, session: &str) -> Result<J> {
    let s = read_state(root)?;
    advise_state(root, path, session, s)
}

fn advise_state(root: &Path, path: &Path, session: &str, s: J) -> Result<J> {
    if advise_session_invalid(&s, session) {
        return Err(guard_error(
            "advice requires the registered app-server session",
        ));
    }
    advise_settled(root, path, s)
}

fn advise_session_invalid(s: &J, session: &str) -> bool {
    [
        model::int(s, "version") != Some(2),
        model::string(s, "session").as_deref() != Some(session),
    ]
    .into_iter()
    .any(|invalid| invalid)
}

fn advise_settled(root: &Path, path: &Path, s: J) -> Result<J> {
    if advise_phase_invalid(&s) {
        return Err(guard_error(
            "advice requires a settled guard without pending delivery",
        ));
    }
    advise_enabled(root, path, s)
}

fn advise_phase_invalid(s: &J) -> bool {
    let phase = model::string(s, "phase").unwrap_or_default();
    [
        !matches!(phase.as_str(), "monitoring" | "requested" | "parked"),
        external_delivery_pending(s),
    ]
    .into_iter()
    .any(|invalid| invalid)
}

fn advise_enabled(root: &Path, path: &Path, s: J) -> Result<J> {
    if !model::bool_or(&s, "advisor_enabled", true) {
        return Ok(json!({"status": "disabled"}));
    }
    advise_read(root, path, s)
}

fn advise_read(root: &Path, path: &Path, s: J) -> Result<J> {
    let raw = util::read_bytes(path)?;
    advise_raw(root, path, s, raw)
}

fn advise_raw(root: &Path, path: &Path, s: J, raw: Vec<u8>) -> Result<J> {
    if raw.len() > 16000 {
        return Err(guard_error("handoff exceeds 16 KB"));
    }
    advise_parse(root, path, s, raw)
}

fn advise_parse(root: &Path, path: &Path, s: J, raw: Vec<u8>) -> Result<J> {
    let handoff: J = serde_json::from_slice(&raw)?;
    advise_validate(root, path, s, handoff)
}

fn advise_validate(root: &Path, path: &Path, s: J, handoff: J) -> Result<J> {
    if advise_draft_invalid(&s, &handoff) {
        return Err(guard_error("draft task/epoch/cwd mismatch"));
    }
    advise_bind(root, path, s, handoff)
}

fn advise_draft_invalid(s: &J, handoff: &J) -> bool {
    [
        !handoff.is_object(),
        ["task", "epoch", "cwd"]
            .iter()
            .any(|key| model::get(handoff, key) != model::get(s, key)),
    ]
    .into_iter()
    .any(|invalid| invalid)
}

fn advise_bind(root: &Path, path: &Path, mut s: J, handoff: J) -> Result<J> {
    let cwd = model::string(&s, "cwd").unwrap_or_default();
    let checkout = fingerprint(&cwd)?;
    let binding = advise_binding(&s, checkout, &handoff);
    let job = advise_job(root, &binding, &handoff);
    let reference = advise_reference(&job, binding, path);
    s["advisor"] = reference;
    advise_save(root, s)
}

fn advise_binding(s: &J, checkout: String, handoff: &J) -> J {
    json!({
        "task": s.get("task").cloned().unwrap_or(Value::Null),
        "epoch": s.get("epoch").cloned().unwrap_or(Value::Null),
        "session": s.get("session").cloned().unwrap_or(Value::Null),
        "role": s.get("role").cloned().unwrap_or(Value::Null),
        "cwd": s.get("cwd").cloned().unwrap_or(Value::Null),
        "app_server_socket": s.get("app_server_socket").cloned().unwrap_or(Value::Null),
        "checkout": checkout,
        "handoff_sha": handoff_digest(handoff)
    })
}

fn advise_job(root: &Path, binding: &J, handoff: &J) -> J {
    crate::codex::advisor::request(root, binding, handoff)
        .unwrap_or_else(|error| json!({"status": "error", "error": error.to_string()}))
}

fn advise_reference(job: &J, binding: J, path: &Path) -> J {
    let mut reference = advise_reference_fields(job);
    reference.insert("binding".to_string(), binding);
    reference.insert(
        "source".to_string(),
        json!(util::resolve_path(path).display().to_string()),
    );
    Value::Object(reference)
}

fn advise_reference_fields(job: &J) -> Map<String, Value> {
    ["id", "path", "result_path", "status", "error"]
        .iter()
        .filter_map(|key| model::get(job, key).map(|value| (key.to_string(), value.clone())))
        .collect()
}

fn advise_save(root: &Path, mut s: J) -> Result<J> {
    save(root, &mut s)?;
    advice_status(root, &s)
}

pub fn advice_status(_root: &Path, s: &J) -> Result<J> {
    let Some(reference) = advice_reference(s) else {
        return Ok(advice_absent(s));
    };
    advice_present(s, &reference)
}

fn advice_reference(s: &J) -> Option<J> {
    model::get(s, "advisor")
        .filter(|value| model::truthy(Some(value)))
        .cloned()
}

fn advice_absent(s: &J) -> J {
    json!({"status": if model::bool_or(s, "advisor_enabled", true) { "not_requested" } else { "disabled" }})
}

fn advice_present(s: &J, reference: &J) -> Result<J> {
    let mut result = advice_base(reference);
    let outcome = advice_enrich(s, reference, &mut result);
    Ok(outcome.map_or_else(|error| advice_unavailable(reference, &error), |()| result))
}

fn advice_base(reference: &J) -> J {
    let result: Map<String, Value> = reference
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(key, _)| key.as_str() != "binding" && key.as_str() != "source")
        .map(|(key, value)| (key.to_string(), value.clone()))
        .collect();
    Value::Object(result)
}

fn advice_unavailable(reference: &J, error: &FlowError) -> J {
    json!({
        "status": "unavailable",
        "id": reference.get("id").cloned().unwrap_or(Value::Null),
        "error": error.to_string()
    })
}

fn advice_enrich(s: &J, reference: &J, result: &mut J) -> Result<()> {
    advice_enrich_result_file(reference, result)?;
    advice_verify(reference, s, result)
}

fn advice_enrich_result_file(reference: &J, result: &mut J) -> Result<()> {
    let Some(path) = model::string(reference, "result_path") else {
        return Ok(());
    };
    advice_merge_result_file(&path, result)
}

fn advice_merge_result_file(path: &str, result: &mut J) -> Result<()> {
    if !Path::new(path).exists() {
        return Ok(());
    }
    advice_merge_json(path, result)
}

fn advice_merge_json(path: &str, result: &mut J) -> Result<()> {
    let extra = util::read_json(Path::new(path))?;
    advice_merge_extra(extra, result);
    Ok(())
}

fn advice_merge_extra(extra: J, result: &mut J) {
    extra
        .as_object()
        .into_iter()
        .flatten()
        .for_each(|(key, value)| {
            result[key] = value.clone();
        });
}

fn advice_verify(reference: &J, s: &J, result: &mut J) -> Result<()> {
    let binding = advice_binding(reference)?;
    advice_verify_current(reference, &binding, s, result)
}

fn advice_binding(reference: &J) -> Result<J> {
    model::get(reference, "binding")
        .cloned()
        .ok_or_else(|| guard_error("advisor binding missing"))
}

fn advice_verify_current(reference: &J, binding: &J, s: &J, result: &mut J) -> Result<()> {
    if advice_fields_mismatch(s, binding) {
        return advice_mark_stale(reference, result);
    }
    advice_verify_checkout(reference, binding, s, result)
}

fn advice_fields_mismatch(s: &J, binding: &J) -> bool {
    [
        "task",
        "epoch",
        "session",
        "role",
        "cwd",
        "app_server_socket",
    ]
    .iter()
    .any(|key| model::get(s, key) != model::get(binding, key))
}

fn advice_verify_checkout(reference: &J, binding: &J, s: &J, result: &mut J) -> Result<()> {
    let current = fingerprint(&model::string(s, "cwd").unwrap_or_default())?;
    advice_compare_checkout(reference, binding, current, result)
}

fn advice_compare_checkout(
    reference: &J,
    binding: &J,
    current: String,
    result: &mut J,
) -> Result<()> {
    if current != model::string(binding, "checkout").unwrap_or_default() {
        return advice_mark_stale(reference, result);
    }
    advice_verify_source(reference, binding, result)
}

fn advice_verify_source(reference: &J, binding: &J, result: &mut J) -> Result<()> {
    let current = advice_source_digest(reference)?;
    advice_compare_source(reference, binding, current, result)
}

fn advice_source_digest(reference: &J) -> Result<String> {
    let source = PathBuf::from(model::string(reference, "source").unwrap_or_default());
    Ok(handoff_digest(&util::read_json(&source)?))
}

fn advice_compare_source(
    reference: &J,
    binding: &J,
    current: String,
    result: &mut J,
) -> Result<()> {
    if current != model::string(binding, "handoff_sha").unwrap_or_default() {
        return advice_mark_stale(reference, result);
    }
    Ok(())
}

fn advice_mark_stale(reference: &J, result: &mut J) -> Result<()> {
    *result = json!({
        "status": "stale",
        "id": reference.get("id").cloned().unwrap_or(Value::Null),
        "result_path": reference.get("result_path").cloned().unwrap_or(Value::Null)
    });
    Ok(())
}

pub fn wait_advice(root: &Path, seconds: f64) -> Result<J> {
    let deadline = Instant::now() + Duration::from_secs_f64(seconds);
    wait_advice_until(root, deadline)
}

fn wait_advice_until(root: &Path, deadline: Instant) -> Result<J> {
    let mut result = None;
    (|| -> Result<()> {
        while result.is_none() {
            result = wait_advice_step(root, deadline)?;
        }
        Ok(())
    })()?;
    Ok(result.unwrap_or(Value::Null))
}

fn wait_advice_step(root: &Path, deadline: Instant) -> Result<Option<J>> {
    let state = read_state(root)?;
    wait_advice_status(root, deadline, &state)
}

fn wait_advice_status(root: &Path, deadline: Instant, state: &J) -> Result<Option<J>> {
    let result = advice_status(root, state)?;
    wait_advice_finish(result, deadline)
}

fn wait_advice_finish(result: J, deadline: Instant) -> Result<Option<J>> {
    if wait_advice_keep_waiting(&result, deadline) {
        wait_advice_sleep(deadline);
        return Ok(None);
    }
    Ok(Some(result))
}

fn wait_advice_keep_waiting(result: &J, deadline: Instant) -> bool {
    [wait_advice_pending(result), Instant::now() < deadline]
        .into_iter()
        .all(|waiting| waiting)
}

fn wait_advice_pending(result: &J) -> bool {
    matches!(
        model::string(result, "status").as_deref(),
        Some("queued" | "running")
    )
}

fn wait_advice_sleep(deadline: Instant) {
    let remaining = deadline.saturating_duration_since(Instant::now());
    thread::sleep(remaining.min(Duration::from_millis(200)));
}

pub fn diagnosis(s: &J, watcher: &J) -> J {
    if !model::bool_or(watcher, "healthy", false) {
        return diagnosis_watcher_unhealthy(watcher);
    }
    let phase = model::string(s, "phase").unwrap_or_else(|| "unknown".to_string());
    diagnosis_phase(s, &phase)
}

fn diagnosis_watcher_unhealthy(watcher: &J) -> J {
    json!({
        "classification": "watcher_unhealthy",
        "reason": model::string(watcher, "reason").unwrap_or_else(|| "unknown".to_string()),
        "next_step": model::string(watcher, "next_step").unwrap_or_else(|| "inspect watch.log and the process identity; restart only the verified watcher; do not reuse an old guard".to_string())
    })
}

fn diagnosis_phase(s: &J, phase: &str) -> J {
    if phase == "needs_attention" {
        return diagnosis_needs_attention(s);
    }
    diagnosis_delivery(s, phase)
}

fn diagnosis_needs_attention(s: &J) -> J {
    json!({
        "classification": "needs_attention",
        "reason": model::string(s, "error").unwrap_or_else(|| "guard needs attention".to_string()),
        "next_step": "inspect the recorded error and current pane/session; reconcile only the existing state; never replay an ambiguous action"
    })
}

fn diagnosis_delivery(s: &J, phase: &str) -> J {
    if external_delivery_pending(s) {
        return json!({
            "classification": "external_delivery",
            "reason": "a queue delivery is pending",
            "next_step": "ACK or explicitly resolve the existing delivery before checkpointing; do not resend"
        });
    }
    diagnosis_rotation(s, phase)
}

fn diagnosis_rotation(s: &J, phase: &str) -> J {
    if model::bool_or(s, "rotation_due", false) {
        return json!({
            "classification": "rotation_due",
            "reason": "context threshold reached",
            "next_step": "finish a safe checkpoint and rotate; do not dispatch into this role during handoff"
        });
    }
    diagnosis_mutating(s, phase)
}

fn diagnosis_mutating(s: &J, phase: &str) -> J {
    if matches!(
        phase,
        "request_sending"
            | "review_config_sending"
            | "runtime_restore_sending"
            | "tui_launch_sending"
    ) {
        return json!({
            "classification": phase,
            "reason": "a mutating action is in flight",
            "next_step": "inspect the exact submission and current runtime; use the matching reconcile path only after inspection; never replay"
        });
    }
    diagnosis_successor(s, phase)
}

fn diagnosis_successor(s: &J, phase: &str) -> J {
    if matches!(
        phase,
        "awaiting_shell" | "awaiting_tui" | "successor_ready" | "awaiting_ack"
    ) {
        return json!({
            "classification": phase,
            "reason": "successor startup or acknowledgement is in progress",
            "next_step": "inspect the successor pane, bridge marker and exact session; do not launch over another process or reuse the predecessor"
        });
    }
    diagnosis_requested(s, phase)
}

fn diagnosis_requested(s: &J, phase: &str) -> J {
    if phase == "requested" {
        return json!({
            "classification": phase,
            "reason": "a checkpoint/rotation request is recorded",
            "next_step": "prepare the handoff at the next safe boundary; do not interrupt an active turn"
        });
    }
    diagnosis_parked(s, phase)
}

fn diagnosis_parked(s: &J, phase: &str) -> J {
    if phase == "parked" {
        return json!({
            "classification": "parked",
            "reason": "worker is parked after a completed assignment",
            "next_step": "wait for a new pending assignment ID; do not reuse the completed assignment"
        });
    }
    diagnosis_released(s, phase)
}

fn diagnosis_released(s: &J, phase: &str) -> J {
    if phase == "released" {
        return json!({
            "classification": "released",
            "reason": "guard is released",
            "next_step": "register a new guard only for a new task/assignment; do not resume the released session implicitly"
        });
    }
    diagnosis_telemetry(s, phase)
}

fn diagnosis_telemetry(s: &J, phase: &str) -> J {
    if model::get(s, "usage").map(Value::is_null).unwrap_or(true) {
        return json!({
            "classification": "telemetry_unknown",
            "reason": "context usage is unknown",
            "next_step": "inspect the transcript identity and guard observation; do not claim automatic protection or raise thresholds"
        });
    }
    diagnosis_healthy(phase)
}

fn diagnosis_healthy(phase: &str) -> J {
    json!({
        "classification": if phase == "monitoring" { "monitoring" } else { phase },
        "reason": "guard is healthy",
        "next_step": "no action; continue monitoring or dispatch within the current authorization"
    })
}

pub fn checkpoint(root: &Path, path: &Path, session: &str) -> Result<J> {
    let s = read_state(root)?;
    checkpoint_state(root, path, session, s)
}

fn checkpoint_state(root: &Path, path: &Path, session: &str, s: J) -> Result<J> {
    checkpoint_version(&s)?;
    checkpoint_ready(root, path, session, s)
}

fn checkpoint_version(s: &J) -> Result<()> {
    if model::int(s, "version") == Some(2) {
        return Ok(());
    }
    Err(guard_error("legacy guard requires explicit migrate"))
}

fn checkpoint_ready(root: &Path, path: &Path, session: &str, s: J) -> Result<J> {
    checkpoint_session_phase(&s, session)?;
    checkpoint_handoff(root, path, session, s)
}

fn checkpoint_session_phase(s: &J, session: &str) -> Result<()> {
    [
        model::string(s, "session").as_deref() == Some(session),
        matches!(
            model::string(s, "phase").as_deref(),
            Some("monitoring" | "requested" | "parked")
        ),
    ]
    .into_iter()
    .all(|valid| valid)
    .then_some(())
    .ok_or_else(|| guard_error("checkpoint session or phase mismatch"))
}

fn checkpoint_handoff(root: &Path, path: &Path, session: &str, s: J) -> Result<J> {
    if external_delivery_pending(&s) {
        return Err(guard_error(
            "acknowledge pending wait-queue delivery before checkpoint",
        ));
    }
    checkpoint_verify(root, path, session, s)
}

fn checkpoint_verify(root: &Path, path: &Path, session: &str, s: J) -> Result<J> {
    let raw = verify_handoff(&s, path)?;
    checkpoint_parse(root, session, s, raw)
}

fn checkpoint_parse(root: &Path, session: &str, s: J, raw: Vec<u8>) -> Result<J> {
    let handoff: J = serde_json::from_slice(&raw)?;
    checkpoint_assignment(root, session, s, handoff)
}

fn checkpoint_assignment(root: &Path, session: &str, s: J, handoff: J) -> Result<J> {
    let assignment = model::get(&handoff, "assignment").cloned();
    checkpoint_assignment_check(&s, assignment.as_ref())?;
    checkpoint_transcript(root, session, s, handoff, assignment)
}

fn checkpoint_assignment_check(s: &J, assignment: Option<&J>) -> Result<()> {
    let completed = checkpoint_completed_ids(s);
    checkpoint_assignment_status(s, assignment, &completed)
}

fn checkpoint_completed_ids(s: &J) -> Vec<String> {
    model::array_field(s, "completed_assignments")
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(ToString::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn checkpoint_assignment_status(s: &J, assignment: Option<&J>, completed: &[String]) -> Result<()> {
    let Some(assignment) = checkpoint_assignment_value(assignment) else {
        return checkpoint_parked_requires_assignment(s);
    };
    checkpoint_pending_assignment(assignment, completed)
}

fn checkpoint_assignment_value(assignment: Option<&J>) -> Option<&J> {
    assignment.filter(|value| !value.is_null())
}

fn checkpoint_pending_assignment(assignment: &J, completed: &[String]) -> Result<()> {
    let id = model::string(assignment, "id").unwrap_or_default();
    let blocked = [
        model::string(assignment, "status").as_deref() == Some("pending"),
        completed.contains(&id),
    ]
    .into_iter()
    .all(|hit| hit);
    if blocked {
        return Err(guard_error(
            "completed assignment cannot be resumed; use a new assignment ID",
        ));
    }
    Ok(())
}

fn checkpoint_parked_requires_assignment(s: &J) -> Result<()> {
    if model::string(s, "phase").as_deref() == Some("parked") {
        return Err(guard_error(
            "parked worker requires an explicit new assignment",
        ));
    }
    Ok(())
}

fn checkpoint_transcript(
    root: &Path,
    session: &str,
    s: J,
    handoff: J,
    assignment: Option<J>,
) -> Result<J> {
    let transcript_path =
        model::string(&s, "transcript").ok_or_else(|| guard_error("transcript is unavailable"))?;
    checkpoint_transcript_data(root, session, s, handoff, assignment, &transcript_path)
}

fn checkpoint_transcript_data(
    root: &Path,
    session: &str,
    s: J,
    handoff: J,
    assignment: Option<J>,
    transcript_path: &str,
) -> Result<J> {
    let data = transcript_state(Path::new(transcript_path), None)?;
    checkpoint_transcript_identity(root, session, s, handoff, assignment, data)
}

fn checkpoint_transcript_identity(
    root: &Path,
    session: &str,
    s: J,
    handoff: J,
    assignment: Option<J>,
    data: J,
) -> Result<J> {
    if model::string(&data, "session").as_deref() != Some(session) {
        return Err(guard_error("transcript changed"));
    }
    checkpoint_write(root, s, handoff, assignment, data)
}

fn checkpoint_write(root: &Path, s: J, handoff: J, assignment: Option<J>, data: J) -> Result<J> {
    let handoff_path = util::resolve_path(root).join("handoff.json");
    checkpoint_store_handoff(&handoff_path, &handoff)?;
    checkpoint_after_store(root, s, &handoff_path, assignment, data)
}

fn checkpoint_store_handoff(handoff_path: &Path, handoff: &J) -> Result<()> {
    util::atomic_write_json(handoff_path, handoff)
}

fn checkpoint_after_store(
    root: &Path,
    mut s: J,
    handoff_path: &Path,
    assignment: Option<J>,
    data: J,
) -> Result<J> {
    checkpoint_advisor_source(&mut s, handoff_path);
    checkpoint_set_digest(&mut s, handoff_path)?;
    checkpoint_set_checkout(root, s, assignment, data)
}

fn checkpoint_advisor_source(s: &mut J, handoff_path: &Path) {
    if model::get(s, "advisor")
        .map(|value| model::truthy(Some(value)))
        .unwrap_or(false)
    {
        s["advisor"]["source"] = json!(handoff_path.display().to_string());
    }
}

fn checkpoint_set_digest(s: &mut J, handoff_path: &Path) -> Result<()> {
    s["handoff_sha"] = json!(digest(&util::read_bytes(handoff_path)?));
    Ok(())
}

fn checkpoint_set_checkout(root: &Path, mut s: J, assignment: Option<J>, data: J) -> Result<J> {
    let checkout = checkpoint_fingerprint(&s)?;
    checkpoint_fields(&mut s, checkout, &data);
    checkpoint_save_completed(root, &mut s, assignment)
}

fn checkpoint_fingerprint(s: &J) -> Result<String> {
    fingerprint(&model::string(s, "cwd").unwrap_or_default())
}

fn checkpoint_fields(s: &mut J, checkout: String, data: &J) {
    s["phase"] = json!("ready");
    s["checkout"] = json!(checkout);
    s["checkpoint_users"] = data
        .get("user_messages")
        .cloned()
        .unwrap_or_else(|| json!(0));
    s["checkpoint_at"] = json!(util::now());
}

fn checkpoint_save_completed(root: &Path, s: &mut J, assignment: Option<J>) -> Result<J> {
    checkpoint_completed_update(s, assignment);
    checkpoint_save(root, s)
}

fn checkpoint_completed_update(s: &mut J, assignment: Option<J>) {
    let Some(assignment) = checkpoint_assignment_value(assignment.as_ref()) else {
        return;
    };
    if model::string(assignment, "status").as_deref() == Some("completed") {
        checkpoint_park(s, assignment);
    }
}

fn checkpoint_park(s: &mut J, assignment: &J) {
    let id = model::string(assignment, "id").unwrap_or_default();
    let mut ids = checkpoint_completed_ids(s);
    checkpoint_push_id(&mut ids, id);
    s["completed_assignments"] = json!(ids);
    s["phase"] = json!("parked");
    s["rotation_due"] = json!(false);
}

fn checkpoint_push_id(ids: &mut Vec<String>, id: String) {
    if !ids.contains(&id) {
        ids.push(id);
    }
}

fn checkpoint_save(root: &Path, s: &mut J) -> Result<J> {
    save(root, s)?;
    Ok(s.clone())
}

pub fn validate_ready(root: &Path, s: &mut J) -> Result<()> {
    let path = util::resolve_path(root).join("handoff.json");
    if digest(&util::read_bytes(&path)?) != model::string(s, "handoff_sha").unwrap_or_default() {
        return Err(guard_error("handoff modified after checkpoint"));
    }
    verify_handoff(s, &path)?;
    if fingerprint(&model::string(s, "cwd").unwrap_or_default())?
        != model::string(s, "checkout").unwrap_or_default()
    {
        return Err(guard_error("checkout changed after checkpoint"));
    }
    let transcript_path =
        model::string(s, "transcript").ok_or_else(|| guard_error("transcript is unavailable"))?;
    let cursor = model::get(s, "ready_scan").cloned();
    let data = transcript_state(Path::new(&transcript_path), cursor.as_ref())?;
    s["ready_scan"] = data.get("cursor").cloned().unwrap_or(Value::Null);
    if model::int(&data, "user_messages").unwrap_or(0)
        != model::int(s, "checkpoint_users").unwrap_or(0)
    {
        return Err(guard_error("new user input after checkpoint; do not reset"));
    }
    Ok(())
}

pub fn validate_fresh_successor(s: &J) -> Result<()> {
    let successor = model::string(s, "successor")
        .ok_or_else(|| guard_error("successor identity is unavailable"))?;
    validate_fresh_sessions(s, &successor)
}

fn validate_fresh_sessions(s: &J, successor: &str) -> Result<()> {
    let sessions_dir = model::string(s, "sessions_dir")
        .ok_or_else(|| guard_error("sessions directory is unavailable"))?;
    let matches = util::files_with_suffix(Path::new(&sessions_dir), &format!("{successor}.jsonl"));
    validate_fresh_matches(s, successor, &matches)
}

fn validate_fresh_matches(s: &J, successor: &str, matches: &[PathBuf]) -> Result<()> {
    if matches.len() > 1 {
        return Err(guard_error("successor transcript identity is ambiguous"));
    }
    validate_fresh_path(s, successor, matches.first())
}

fn validate_fresh_path(s: &J, successor: &str, path: Option<&PathBuf>) -> Result<()> {
    let Some(path) = path else {
        return Ok(());
    };
    validate_fresh_read(s, successor, path)
}

fn validate_fresh_read(s: &J, successor: &str, path: &Path) -> Result<()> {
    let data = transcript_state(path, None)?;
    validate_fresh_data(s, successor, &data)
}

fn validate_fresh_data(s: &J, successor: &str, data: &J) -> Result<()> {
    if validate_fresh_mismatch(s, successor, data) {
        return Err(guard_error(
            "successor already received input before its handoff",
        ));
    }
    Ok(())
}

fn validate_fresh_mismatch(s: &J, successor: &str, data: &J) -> bool {
    [
        model::string(data, "session").as_deref() == Some(successor),
        model::string(data, "cwd") == model::string(s, "cwd"),
        model::int(data, "user_messages").unwrap_or(0) == 0,
    ]
    .into_iter()
    .any(|ok| !ok)
}

fn load_guard_state(root: &Path) -> Result<J> {
    let s = read_state(root)?;
    load_guard_version(root, s)
}

fn load_guard_version(root: &Path, s: J) -> Result<J> {
    if model::int(&s, "version") != Some(2) {
        return Err(guard_error("legacy guard requires explicit migrate"));
    }
    load_guard_binding(root, s)
}

fn load_guard_binding(root: &Path, s: J) -> Result<J> {
    let Some(binding) = model::string(&s, "binding") else {
        return Ok(s);
    };
    load_guard_binding_path(root, s, &binding)
}

fn load_guard_binding_path(root: &Path, s: J, binding: &str) -> Result<J> {
    if !Path::new(binding).exists() {
        return Ok(s);
    }
    load_guard_registration(root, s, binding)
}

fn load_guard_registration(root: &Path, s: J, binding: &str) -> Result<J> {
    let registration = util::read_json(Path::new(binding))?;
    load_guard_registration_check(root, s, &registration)
}

fn load_guard_registration_check(root: &Path, s: J, registration: &J) -> Result<J> {
    if model::string(registration, "state_dir")
        != Some(util::resolve_path(root).display().to_string())
    {
        return Err(guard_error("pane guard registration changed"));
    }
    Ok(s)
}

fn observe_context(root: &Path, s: &mut J, h: &mut dyn GuardAdapter) -> Result<()> {
    let session = model::string(s, "session").unwrap_or_default();
    h.set_state(s);
    observe_verified(root, s, h, &session)
}

fn observe_verified(root: &Path, s: &mut J, h: &mut dyn GuardAdapter, session: &str) -> Result<()> {
    h.verify_tui(session)?;
    observe_transcript(root, s)
}

fn observe_transcript(root: &Path, s: &mut J) -> Result<()> {
    let transcript_path =
        model::string(s, "transcript").ok_or_else(|| guard_error("transcript is unavailable"))?;
    observe_read(root, s, Path::new(&transcript_path))
}

fn observe_read(root: &Path, s: &mut J, transcript_path: &Path) -> Result<()> {
    let cursor = model::get(s, "scan").cloned();
    let data = transcript_state(transcript_path, cursor.as_ref())?;
    observe_data(root, s, data)
}

fn observe_data(root: &Path, s: &mut J, data: J) -> Result<()> {
    s["scan"] = data.get("cursor").cloned().unwrap_or(Value::Null);
    if observe_identity_mismatch(s, &data) {
        return Err(guard_error("transcript identity changed"));
    }
    observe_usage(root, s, &data)
}

fn observe_identity_mismatch(s: &J, data: &J) -> bool {
    [
        model::string(data, "session") != model::string(s, "session"),
        model::string(data, "cwd") != model::string(s, "cwd"),
    ]
    .into_iter()
    .any(|mismatch| mismatch)
}

fn observe_usage(root: &Path, s: &mut J, data: &J) -> Result<()> {
    let usage = data.get("usage").cloned().unwrap_or(Value::Null);
    s["usage"] = usage.clone();
    let level = observe_level(s, &usage);
    observe_apply(root, s, level)
}

fn observe_level(s: &J, usage: &J) -> &'static str {
    if usage.is_null() {
        return "unknown";
    }
    observe_level_known(s, usage)
}

fn observe_level_known(s: &J, usage: &J) -> &'static str {
    let tokens = model::number(usage, "tokens").unwrap_or(0.0);
    let ratio = model::number(usage, "ratio").unwrap_or(0.0);
    let rotate = observe_threshold(s, tokens, ratio, "rotate_tokens", "rotate_ratio");
    let warn = observe_threshold(s, tokens, ratio, "warn_tokens", "warn_ratio");
    observe_level_select(rotate, warn)
}

fn observe_threshold(s: &J, tokens: f64, ratio: f64, tokens_key: &str, ratio_key: &str) -> bool {
    let token_limit = model::number(s, tokens_key).unwrap_or(f64::INFINITY);
    let ratio_limit = model::number(s, ratio_key).unwrap_or(f64::INFINITY);
    [tokens >= token_limit, ratio >= ratio_limit]
        .into_iter()
        .any(|hit| hit)
}

fn observe_level_select(rotate: bool, warn: bool) -> &'static str {
    let index = [rotate, warn].into_iter().position(|hit| hit).unwrap_or(2);
    ["rotate", "warn", "normal"][index]
}

fn observe_apply(root: &Path, s: &mut J, level: &str) -> Result<()> {
    s["level"] = json!(level);
    if level == "rotate" {
        s["rotation_due"] = json!(true);
    }
    save(root, s)
}

fn validate_path_for_shell(path: &Path) -> Result<String> {
    let text = path.display().to_string();
    if text
        .chars()
        .any(|character| matches!(character, ';' | '&' | '|' | '$' | '`' | '\n' | '\r'))
    {
        logging::log_event(
            "guard_path_rejected",
            "context_guard",
            "path contains shell metacharacters",
        );
        return Err(guard_error("path contains shell metacharacters"));
    }
    let resolved = util::resolve_path(path);
    if !resolved.is_absolute() {
        logging::log_event(
            "guard_path_rejected",
            "context_guard",
            "path must be absolute",
        );
        return Err(guard_error(format!("path must be absolute: {text}")));
    }
    Ok(resolved.display().to_string())
}

fn request_handoff(root: &Path, s: &mut J, h: &mut dyn GuardAdapter) -> Result<()> {
    request_handoff_ready(s, h).map_or_else(Err, |ready| request_handoff_decide(root, s, h, ready))
}

fn request_handoff_decide(
    root: &Path,
    s: &mut J,
    h: &mut dyn GuardAdapter,
    ready: bool,
) -> Result<()> {
    if !ready {
        return Ok(());
    }
    request_handoff_send(root, s, h)
}

fn request_handoff_ready(s: &J, h: &mut dyn GuardAdapter) -> Result<bool> {
    if !request_handoff_flags(s) {
        return Ok(false);
    }
    h.idle()
}

fn request_handoff_flags(s: &J) -> bool {
    [
        model::bool_or(s, "rotation_due", false),
        model::bool_or(s, "auto_rotate", false),
    ]
    .into_iter()
    .all(|due| due)
}

fn request_handoff_send(root: &Path, s: &mut J, h: &mut dyn GuardAdapter) -> Result<()> {
    request_handoff_begin(root, s)?;
    request_handoff_prompt(root, s, h)
}

fn request_handoff_begin(root: &Path, s: &mut J) -> Result<()> {
    s["phase"] = json!("request_sending");
    save(root, s)
}

fn request_handoff_prompt(root: &Path, s: &mut J, h: &mut dyn GuardAdapter) -> Result<()> {
    let executable = request_handoff_executable()?;
    request_handoff_root(root, s, h, executable)
}

fn request_handoff_executable() -> Result<String> {
    validate_path_for_shell(&util::current_exe()?)
}

fn request_handoff_root(
    root: &Path,
    s: &mut J,
    h: &mut dyn GuardAdapter,
    executable: String,
) -> Result<()> {
    let root_safe = validate_path_for_shell(root)?;
    request_handoff_prompt_inner(root, s, h, &executable, &root_safe)
}

fn request_handoff_prompt_inner(
    root: &Path,
    s: &mut J,
    h: &mut dyn GuardAdapter,
    executable: &str,
    root_safe: &str,
) -> Result<()> {
    h.set_state(s);
    let turn = h.prompt(&format!(
        "Context guard requests a safe handoff for task {}, epoch {}. Read the context-rotation reference installed with this skill. Finish or transfer active commands, release writes, then write a concise handoff and run: {} guard checkpoint --state-dir {} --handoff HANDOFF. Preserve authorization and next action. After checkpoint, end your turn and do not resume work; the guard will start a blank session.",
        model::string(s, "task").unwrap_or_default(),
        model::int(s, "epoch").unwrap_or(0),
        util::shell_quote(executable),
        util::shell_quote(root_safe)
    ))?;
    request_handoff_finish(root, s, turn)
}

fn request_handoff_finish(root: &Path, s: &mut J, turn: String) -> Result<()> {
    s["request_turn"] = json!(turn);
    s["phase"] = json!("requested");
    s["delivery_at"] = json!(util::now());
    save(root, s)
}

fn start_rotation(root: &Path, s: &mut J, h: &mut dyn GuardAdapter) -> Result<()> {
    if !(model::bool_or(s, "auto_rotate", false) && h.idle()?) {
        return Ok(());
    }
    validate_ready(root, s)?;
    h.set_state(s);
    s["before_runtime"] = h.snapshot(None)?;
    validate_ready(root, s)?;
    s["phase"] = json!("tui_exit_sending");
    save(root, s)?;
    h.set_state(s);
    h.exit_tui()?;
    s["phase"] = json!("awaiting_shell");
    s["transition_deadline"] = json!(util::now() + 60.0);
    save(root, s)
}

fn await_shell(root: &Path, s: &mut J, h: &mut dyn GuardAdapter) -> Result<()> {
    if !model::bool_or(s, "auto_rotate", false) {
        return Ok(());
    }
    if util::now() > model::number(s, "transition_deadline").unwrap_or(0.0) {
        return Err(guard_error(
            "TUI exit overdue; inspect without replaying /quit",
        ));
    }
    h.set_state(s);
    if h.shell_ready()? {
        validate_ready(root, s)?;
        s["phase"] = json!("tui_launch_sending");
        s["bridge_nonce"] = json!(uuid_hex());
        save(root, s)?;
        h.set_state(s);
        h.launch_tui(root, &model::string(s, "bridge_nonce").unwrap_or_default())?;
        s["phase"] = json!("awaiting_tui");
        s["transition_deadline"] = json!(util::now() + 120.0);
        save(root, s)?;
    }
    Ok(())
}

fn await_tui(root: &Path, s: &mut J, h: &mut dyn GuardAdapter) -> Result<()> {
    if !model::bool_or(s, "auto_rotate", false) {
        return Ok(());
    }
    if util::now() > model::number(s, "transition_deadline").unwrap_or(0.0) {
        return Err(guard_error(
            "successor TUI protocol readiness overdue; inspect bridge marker",
        ));
    }
    h.set_state(s);
    let Some(session) = h.ready_tui()? else {
        return Ok(());
    };
    if model::string(s, "session").as_deref() == Some(session.as_str()) {
        return Err(guard_error("fresh TUI reused predecessor session"));
    }
    s["successor"] = json!(session);
    s["phase"] = json!("runtime_restore_sending");
    save(root, s)?;
    h.set_state(s);
    h.restore_runtime(s.get("before_runtime").unwrap_or(&Value::Null), &session)?;
    let identity = h.verify_tui(&session)?;
    s["phase"] = json!("successor_ready");
    s["tui_process"] = identity;
    save(root, s)
}

fn resume_successor(root: &Path, s: &mut J, h: &mut dyn GuardAdapter) -> Result<()> {
    resume_successor_ready(s, h)
        .map_or_else(Err, |ready| resume_successor_decide(root, s, h, ready))
}

fn resume_successor_decide(
    root: &Path,
    s: &mut J,
    h: &mut dyn GuardAdapter,
    ready: bool,
) -> Result<()> {
    if !ready {
        return Ok(());
    }
    resume_successor_run(root, s, h)
}

fn resume_successor_ready(s: &J, h: &mut dyn GuardAdapter) -> Result<bool> {
    if !model::bool_or(s, "auto_rotate", false) {
        return Ok(false);
    }
    h.idle()
}

fn resume_successor_run(root: &Path, s: &mut J, h: &mut dyn GuardAdapter) -> Result<()> {
    h.set_state(s);
    resume_successor_verify(root, s, h)
}

fn resume_successor_verify(root: &Path, s: &mut J, h: &mut dyn GuardAdapter) -> Result<()> {
    resume_successor_runtime(s, h)?;
    resume_successor_validate(root, s, h)
}

fn resume_successor_runtime(s: &J, h: &mut dyn GuardAdapter) -> Result<()> {
    crate::herdr::same_runtime(
        s.get("before_runtime").unwrap_or(&Value::Null),
        &h.snapshot(None)?,
    )
}

fn resume_successor_validate(root: &Path, s: &mut J, h: &mut dyn GuardAdapter) -> Result<()> {
    validate_fresh_successor(s)?;
    resume_successor_ready_state(root, s, h)
}

fn resume_successor_ready_state(root: &Path, s: &mut J, h: &mut dyn GuardAdapter) -> Result<()> {
    validate_ready(root, s)?;
    resume_successor_begin(root, s, h)
}

fn resume_successor_begin(root: &Path, s: &mut J, h: &mut dyn GuardAdapter) -> Result<()> {
    s["phase"] = json!("resume_sending");
    save(root, s)?;
    resume_successor_prompt(root, s, h)
}

fn resume_successor_prompt(root: &Path, s: &mut J, h: &mut dyn GuardAdapter) -> Result<()> {
    let executable = resume_successor_executable()?;
    resume_successor_root(root, s, h, executable)
}

fn resume_successor_executable() -> Result<String> {
    validate_path_for_shell(&util::current_exe()?)
}

fn resume_successor_root(
    root: &Path,
    s: &mut J,
    h: &mut dyn GuardAdapter,
    executable: String,
) -> Result<()> {
    let root_safe = validate_path_for_shell(root)?;
    resume_successor_prompt_inner(root, s, h, &executable, &root_safe)
}

fn resume_successor_prompt_inner(
    root: &Path,
    s: &mut J,
    h: &mut dyn GuardAdapter,
    executable: &str,
    root_safe: &str,
) -> Result<()> {
    h.set_state(s);
    let turn = h.prompt(&format!(
        "You are the successor {} for task {}. Read {root_safe}/handoff.json and {root_safe}/state.json; do not load the old conversation. Read only applicable instructions and handoff references. Verify checkout identity and preserve prior authorization. Before any task mutation, run: {} guard ack --state-dir {}. On a state-lock-busy error retry after one second without task writes. On other ACK failures stop and report. After ACK continue next_action within existing scope. Do not repeat completed validation solely because this is a fresh session.",
        model::string(s, "role").unwrap_or_default(),
        model::string(s, "task").unwrap_or_default(),
        util::shell_quote(executable),
        util::shell_quote(root_safe)
    ))?;
    resume_successor_finish(root, s, turn)
}

fn resume_successor_finish(root: &Path, s: &mut J, turn: String) -> Result<()> {
    s["resume_turn"] = json!(turn);
    s["phase"] = json!("awaiting_ack");
    s["delivery_at"] = json!(util::now());
    save(root, s)
}

fn handle_phase(root: &Path, s: &mut J, h: &mut dyn GuardAdapter) -> Result<()> {
    match model::string(s, "phase").as_deref() {
        Some("monitoring") => request_handoff(root, s, h),
        Some("ready") => start_rotation(root, s, h),
        Some("awaiting_shell") => await_shell(root, s, h),
        Some("awaiting_tui") => await_tui(root, s, h),
        Some("successor_ready") => resume_successor(root, s, h),
        _ => Ok(()),
    }
}

pub fn once(root: &Path, h: &mut dyn GuardAdapter) -> Result<J> {
    let root = util::resolve_path(root);
    let s = load_guard_state(&root)?;
    once_state(&root, h, s)
}

fn once_state(root: &Path, h: &mut dyn GuardAdapter, s: J) -> Result<J> {
    if once_terminal(&s) {
        return Ok(s);
    }
    let phase = model::string(&s, "phase").unwrap_or_default();
    once_review_pending(root, h, s, &phase)
}

fn once_terminal(s: &J) -> bool {
    matches!(
        model::string(s, "phase").as_deref(),
        Some("needs_attention" | "released")
    )
}

fn once_review_pending(root: &Path, h: &mut dyn GuardAdapter, s: J, phase: &str) -> Result<J> {
    if phase == "review_config_sending" {
        return Err(guard_error(
            "review configuration is ambiguous; inspect then review-reconcile without replay",
        ));
    }
    once_parked(root, h, s, phase)
}

fn once_parked(root: &Path, h: &mut dyn GuardAdapter, mut s: J, phase: &str) -> Result<J> {
    model::remove(&mut s, "pause_reason");
    if phase == "parked" {
        return Ok(s);
    }
    once_observe(root, h, s, phase)
}

fn once_observe(root: &Path, h: &mut dyn GuardAdapter, s: J, phase: &str) -> Result<J> {
    h.set_state(&s);
    if once_should_observe(phase) {
        return once_observe_active(root, h, s);
    }
    once_delivery(root, h, s)
}

fn once_should_observe(phase: &str) -> bool {
    matches!(phase, "monitoring" | "requested" | "ready")
}

fn once_observe_active(root: &Path, h: &mut dyn GuardAdapter, mut s: J) -> Result<J> {
    observe_context(root, &mut s, h)?;
    once_delivery(root, h, s)
}

fn once_delivery(root: &Path, h: &mut dyn GuardAdapter, s: J) -> Result<J> {
    if external_delivery_pending(&s) {
        return once_pause(root, s);
    }
    once_handle(root, h, s)
}

fn once_pause(root: &Path, mut s: J) -> Result<J> {
    s["pause_reason"] = json!("wait_queue_delivery_pending");
    save(root, &mut s)?;
    Ok(s)
}

fn once_handle(root: &Path, h: &mut dyn GuardAdapter, mut s: J) -> Result<J> {
    h.set_state(&s);
    handle_phase(root, &mut s, h)?;
    once_after_phase(h, s)
}

fn once_after_phase(h: &mut dyn GuardAdapter, s: J) -> Result<J> {
    h.set_state(&s);
    if once_ambiguous(&s) {
        return Err(guard_error(
            "delivery is ambiguous; inspect and reconcile, never blindly replay",
        ));
    }
    once_overdue(&s)
}

fn once_ambiguous(s: &J) -> bool {
    matches!(
        model::string(s, "phase").as_deref(),
        Some(
            "request_sending"
                | "tui_exit_sending"
                | "tui_launch_sending"
                | "runtime_restore_sending"
                | "resume_sending"
        )
    )
}

fn once_overdue(s: &J) -> Result<J> {
    if once_handoff_overdue(s) {
        return Err(guard_error(
            "handoff or acknowledgement overdue; inspect without resending",
        ));
    }
    Ok(s.clone())
}

fn once_handoff_overdue(s: &J) -> bool {
    [
        matches!(
            model::string(s, "phase").as_deref(),
            Some("requested" | "awaiting_ack")
        ),
        util::now() - model::number(s, "delivery_at").unwrap_or(0.0) > 600.0,
    ]
    .into_iter()
    .all(|overdue| overdue)
}

pub fn reconcile(root: &Path, session: &str, h: &mut dyn GuardAdapter, shell: bool) -> Result<J> {
    let root = util::resolve_path(root);
    let s = read_state(&root)?;
    reconcile_state(root, session, h, shell, s)
}

fn reconcile_state(
    root: PathBuf,
    session: &str,
    h: &mut dyn GuardAdapter,
    shell: bool,
    s: J,
) -> Result<J> {
    reconcile_phase(&s)?;
    h.set_state(&s);
    let prior = model::string(&s, "phase_before_error").unwrap_or_default();
    reconcile_prior(root, session, h, shell, s, &prior)
}

fn reconcile_phase(s: &J) -> Result<()> {
    if model::string(s, "phase").as_deref() == Some("needs_attention") {
        return Ok(());
    }
    Err(guard_error(
        "reconcile only applies after an inspected error",
    ))
}

fn reconcile_prior(
    root: PathBuf,
    session: &str,
    h: &mut dyn GuardAdapter,
    shell: bool,
    s: J,
    prior: &str,
) -> Result<J> {
    if prior == "review_config_sending" {
        return Err(guard_error(
            "use review-reconcile for an ambiguous approval configuration",
        ));
    }
    reconcile_branch(root, session, h, shell, s, prior)
}

fn reconcile_branch(
    root: PathBuf,
    session: &str,
    h: &mut dyn GuardAdapter,
    shell: bool,
    s: J,
    prior: &str,
) -> Result<J> {
    if shell {
        return reconcile_shell(root, h, s, prior);
    }
    reconcile_thread(root, session, h, s, prior)
}

fn reconcile_history_path(root: &Path) -> PathBuf {
    root.join("history")
        .join(format!("recovery-{}.json", util::now_ns()))
}

fn reconcile_record(root: &Path, s: &J) -> Result<()> {
    util::atomic_write_json(&reconcile_history_path(root), s)
}

fn reconcile_finish(root: &Path, mut s: J) -> Result<J> {
    model::remove(&mut s, "error");
    model::remove(&mut s, "phase_before_error");
    save(root, &mut s)?;
    Ok(s)
}

fn reconcile_shell(root: PathBuf, h: &mut dyn GuardAdapter, s: J, prior: &str) -> Result<J> {
    reconcile_shell_prior(prior)?;
    h.set_state(&s);
    reconcile_shell_ready(root, h, s)
}

fn reconcile_shell_prior(prior: &str) -> Result<()> {
    if matches!(
        prior,
        "tui_exit_sending" | "awaiting_shell" | "tui_launch_sending" | "awaiting_tui"
    ) {
        return Ok(());
    }
    Err(guard_error(
        "shell recovery cannot discard an in-flight work delivery",
    ))
}

fn reconcile_shell_ready(root: PathBuf, h: &mut dyn GuardAdapter, s: J) -> Result<J> {
    let ready = h.shell_ready()?;
    reconcile_shell_confirmed(root, h, s, ready)
}

fn reconcile_shell_confirmed(
    root: PathBuf,
    h: &mut dyn GuardAdapter,
    s: J,
    ready: bool,
) -> Result<J> {
    if !ready {
        return Err(guard_error(
            "shell recovery requires a confirmed empty foreground shell",
        ));
    }
    reconcile_shell_validate(root, h, s)
}

fn reconcile_shell_validate(root: PathBuf, _h: &mut dyn GuardAdapter, mut s: J) -> Result<J> {
    validate_ready(&root, &mut s)?;
    reconcile_shell_write(&root, s)
}

fn reconcile_shell_write(root: &Path, s: J) -> Result<J> {
    reconcile_record(root, &s)?;
    reconcile_shell_finish(root, s)
}

fn reconcile_shell_finish(root: &Path, mut s: J) -> Result<J> {
    s["phase"] = json!("awaiting_shell");
    s["auto_rotate"] = json!(false);
    s["transition_deadline"] = json!(util::now() + 120.0);
    model::remove(&mut s, "successor");
    model::remove(&mut s, "bridge_nonce");
    reconcile_finish(root, s)
}

fn reconcile_thread(
    root: PathBuf,
    session: &str,
    h: &mut dyn GuardAdapter,
    s: J,
    prior: &str,
) -> Result<J> {
    let actual = reconcile_actual(&s, h)?;
    reconcile_thread_session(root, session, h, s, prior, actual)
}

fn reconcile_actual(s: &J, h: &mut dyn GuardAdapter) -> Result<String> {
    if reconcile_bridge_pending(s) {
        return reconcile_ready_tui(h);
    }
    h.session()
}

fn reconcile_bridge_pending(s: &J) -> bool {
    model::get(s, "bridge_nonce")
        .map(|value| model::truthy(Some(value)))
        .unwrap_or(false)
}

fn reconcile_ready_tui(h: &mut dyn GuardAdapter) -> Result<String> {
    Ok(h.ready_tui()?.unwrap_or_default())
}

fn reconcile_thread_session(
    root: PathBuf,
    session: &str,
    h: &mut dyn GuardAdapter,
    s: J,
    prior: &str,
    actual: String,
) -> Result<J> {
    reconcile_thread_matches(session, &actual)?;
    reconcile_thread_record(root, h, s, prior, actual)
}

fn reconcile_thread_matches(session: &str, actual: &str) -> Result<()> {
    if actual == session {
        return Ok(());
    }
    Err(guard_error(
        "--session must match the inspected live thread",
    ))
}

fn reconcile_thread_record(
    root: PathBuf,
    h: &mut dyn GuardAdapter,
    mut s: J,
    prior: &str,
    actual: String,
) -> Result<J> {
    reconcile_record(&root, &s)?;
    reconcile_thread_apply(&root, &mut s, h, prior, actual)
}

fn reconcile_thread_apply(
    root: &Path,
    s: &mut J,
    h: &mut dyn GuardAdapter,
    prior: &str,
    actual: String,
) -> Result<J> {
    if model::string(s, "session").as_deref() == Some(actual.as_str()) {
        return reconcile_same_session(root, s.clone());
    }
    reconcile_successor(root, s.clone(), h, prior, actual)
}

fn reconcile_same_session(root: &Path, mut s: J) -> Result<J> {
    s["phase"] = json!("monitoring");
    s["auto_rotate"] = json!(false);
    s["rotation_due"] = json!(true);
    reconcile_finish(root, s)
}

fn reconcile_successor(
    root: &Path,
    mut s: J,
    h: &mut dyn GuardAdapter,
    prior: &str,
    actual: String,
) -> Result<J> {
    validate_ready(root, &mut s)?;
    reconcile_successor_state(root, s, h, prior, actual)
}

fn reconcile_successor_state(
    root: &Path,
    s: J,
    h: &mut dyn GuardAdapter,
    prior: &str,
    actual: String,
) -> Result<J> {
    let mut successor_state = s.clone();
    successor_state["successor"] = json!(actual.clone());
    h.set_state(&successor_state);
    reconcile_successor_runtime(&s, h, &actual)?;
    reconcile_successor_fields(root, s, h, prior, actual)
}

fn reconcile_successor_runtime(s: &J, h: &mut dyn GuardAdapter, actual: &str) -> Result<()> {
    crate::herdr::same_runtime(
        s.get("before_runtime").unwrap_or(&Value::Null),
        &h.snapshot(Some(actual))?,
    )
}

fn reconcile_successor_fields(
    root: &Path,
    mut s: J,
    h: &mut dyn GuardAdapter,
    prior: &str,
    actual: String,
) -> Result<J> {
    let identity = h.verify_tui(&actual)?;
    s["successor"] = json!(actual);
    s["tui_process"] = identity;
    s["auto_rotate"] = json!(false);
    reconcile_successor_phase(root, s, prior)
}

fn reconcile_successor_phase(root: &Path, s: J, prior: &str) -> Result<J> {
    if matches!(prior, "resume_sending" | "awaiting_ack") {
        return reconcile_awaiting_ack(root, s);
    }
    reconcile_successor_rotation(root, s, prior)
}

fn reconcile_awaiting_ack(root: &Path, mut s: J) -> Result<J> {
    s["phase"] = json!("awaiting_ack");
    s["delivery_at"] = json!(util::now());
    reconcile_finish(root, s)
}

fn reconcile_successor_rotation(root: &Path, s: J, prior: &str) -> Result<J> {
    if ROTATION_PHASES.contains(&prior) {
        return reconcile_successor_ready(root, s);
    }
    Err(guard_error(
        "unregistered successor; inspect lifecycle before recovery",
    ))
}

fn reconcile_successor_ready(root: &Path, mut s: J) -> Result<J> {
    s["phase"] = json!("successor_ready");
    reconcile_finish(root, s)
}

pub fn ack(root: &Path, session: &str) -> Result<J> {
    let root = util::resolve_path(root);
    let s = read_state(&root)?;
    ack_version(root, session, s)
}

fn ack_version(root: PathBuf, session: &str, s: J) -> Result<J> {
    if model::int(&s, "version") != Some(2) {
        return Err(guard_error("legacy guard requires explicit migrate"));
    }
    ack_phase(root, session, s)
}

fn ack_phase(root: PathBuf, session: &str, s: J) -> Result<J> {
    if !ack_phase_ok(&s, session) {
        return Err(guard_error("unexpected successor acknowledgement"));
    }
    ack_integrity(&root, session, s)
}

fn ack_phase_ok(s: &J, session: &str) -> bool {
    [
        model::string(s, "phase").as_deref() == Some("awaiting_ack"),
        model::string(s, "successor").as_deref() == Some(session),
    ]
    .into_iter()
    .all(|ok| ok)
}

fn ack_integrity(root: &Path, session: &str, s: J) -> Result<J> {
    let checkout = ack_checkout(&s)?;
    ack_checkout_compare(root, session, s, checkout)
}

fn ack_checkout(s: &J) -> Result<String> {
    fingerprint(&model::string(s, "cwd").unwrap_or_default())
}

fn ack_checkout_compare(root: &Path, session: &str, s: J, checkout: String) -> Result<J> {
    if checkout != model::string(&s, "checkout").unwrap_or_default() {
        return Err(guard_error(
            "checkout or handoff changed before acknowledgement",
        ));
    }
    ack_handoff(root, session, s)
}

fn ack_handoff(root: &Path, session: &str, s: J) -> Result<J> {
    let actual = ack_handoff_digest(root)?;
    ack_handoff_compare(root, session, s, actual)
}

fn ack_handoff_digest(root: &Path) -> Result<String> {
    Ok(digest(&util::read_bytes(&root.join("handoff.json"))?))
}

fn ack_handoff_compare(root: &Path, session: &str, s: J, actual: String) -> Result<J> {
    if actual != model::string(&s, "handoff_sha").unwrap_or_default() {
        return Err(guard_error(
            "checkout or handoff changed before acknowledgement",
        ));
    }
    ack_transcript(root, session, s)
}

fn ack_transcript(root: &Path, session: &str, s: J) -> Result<J> {
    let sessions_dir = model::string(&s, "sessions_dir")
        .ok_or_else(|| guard_error("sessions directory is unavailable"))?;
    ack_find(root, session, s, &sessions_dir)
}

fn ack_find(root: &Path, session: &str, s: J, sessions_dir: &str) -> Result<J> {
    let successor_transcript = find_transcript(session, Path::new(sessions_dir))?;
    ack_read(root, session, s, &successor_transcript)
}

fn ack_read(root: &Path, session: &str, s: J, successor_transcript: &Path) -> Result<J> {
    let meta = transcript_state(successor_transcript, None)?;
    ack_meta(root, session, s, successor_transcript, &meta)
}

fn ack_meta(root: &Path, session: &str, s: J, successor_transcript: &Path, meta: &J) -> Result<J> {
    if ack_meta_mismatch(&s, session, meta) {
        return Err(guard_error("successor identity mismatch"));
    }
    ack_archive(root, session, s, successor_transcript)
}

fn ack_meta_mismatch(s: &J, session: &str, meta: &J) -> bool {
    [
        model::string(meta, "session").as_deref() == Some(session),
        model::string(meta, "cwd") == model::string(s, "cwd"),
    ]
    .into_iter()
    .any(|ok| !ok)
}

fn ack_archive(root: &Path, session: &str, s: J, successor_transcript: &Path) -> Result<J> {
    let archive = ack_archive_path(root, &s);
    ack_write_archive(&archive, &s)?;
    ack_update(root, session, s, successor_transcript)
}

fn ack_archive_path(root: &Path, s: &J) -> PathBuf {
    root.join("history").join(format!(
        "epoch-{}-{}.json",
        model::int(s, "epoch").unwrap_or(0),
        model::string(s, "session").unwrap_or_default()
    ))
}

fn ack_write_archive(archive: &Path, s: &J) -> Result<()> {
    util::atomic_write_json(archive, s)
}

fn ack_update(root: &Path, session: &str, mut s: J, successor_transcript: &Path) -> Result<J> {
    ack_set_fields(&mut s, session, successor_transcript);
    ack_remove_fields(&mut s);
    ack_save(root, s)
}

fn ack_set_fields(s: &mut J, session: &str, successor_transcript: &Path) {
    s["session"] = json!(session);
    s["transcript"] = json!(successor_transcript.display().to_string());
    s["epoch"] = json!(model::int(s, "epoch").unwrap_or(0) + 1);
    s["phase"] = json!("monitoring");
    s["rotation_due"] = json!(false);
    s["level"] = json!("unknown");
    s["usage"] = Value::Null;
}

fn ack_remove_fields(s: &mut J) {
    [
        "successor",
        "successor_transcript",
        "checkout",
        "checkpoint_users",
        "checkpoint_at",
        "handoff_sha",
        "scan",
        "ready_scan",
    ]
    .into_iter()
    .for_each(|key| {
        model::remove(s, key);
    });
}

fn ack_save(root: &Path, mut s: J) -> Result<J> {
    save(root, &mut s)?;
    Ok(s)
}

pub fn validate_watch(root: &Path) -> Result<()> {
    let s = read_state(root)?;
    validate_watch_version(&s)
}

fn validate_watch_version(s: &J) -> Result<()> {
    if model::int(s, "version") != Some(2) {
        return Err(guard_error("legacy guard requires explicit migrate"));
    }
    validate_watch_phase(s)
}

fn validate_watch_phase(s: &J) -> Result<()> {
    if model::string(s, "phase").as_deref() == Some("released") {
        return Err(guard_error("released guard cannot be watched"));
    }
    Ok(())
}

fn read_state_or_empty(root: &Path) -> J {
    read_state(root).unwrap_or_else(|_| json!({}))
}

fn guard_failure(root: &Path, h: &mut dyn GuardAdapter, notify: bool, error: &FlowError) -> J {
    let s = read_state_or_empty(root);
    if guard_failure_attention(&s) {
        return s;
    }
    let transient = guard_failure_transient(error);
    let message = guard_failure_message(error);
    guard_failure_log(&s, error, transient);
    let phase = model::string(&s, "phase").unwrap_or_default();
    guard_failure_route(root, h, notify, s, phase, message, transient)
}

fn guard_failure_attention(s: &J) -> bool {
    model::string(s, "phase").as_deref() == Some("needs_attention")
}

fn guard_failure_transient(error: &FlowError) -> bool {
    matches!(error.kind, ErrorKind::Transient | ErrorKind::Timeout)
}

fn guard_failure_message(error: &FlowError) -> String {
    if error.message.is_empty() {
        return error.kind.type_name().to_string();
    }
    error.message.clone()
}

fn guard_failure_log(s: &J, error: &FlowError, transient: bool) {
    logging::log_event_fields(
        "guard_poll_failed",
        "context_guard",
        "guard poll failed",
        logging::Fields {
            task_id: model::string(s, "task").as_deref(),
            guard_epoch: model::u64_field(s, "epoch"),
            metadata: Some(&json!({
                "exception": error.kind.type_name(),
                "phase": s.get("phase"),
                "transient": transient
            })),
            ..Default::default()
        },
    );
}

fn guard_failure_route(
    root: &Path,
    h: &mut dyn GuardAdapter,
    notify: bool,
    s: J,
    phase: String,
    message: String,
    transient: bool,
) -> J {
    if guard_failure_observed(transient, &phase) {
        return guard_failure_transient_state(root, h, notify, s, phase, message);
    }
    guard_failure_final(root, h, notify, s, phase, &message)
}

fn guard_failure_observed(transient: bool, phase: &str) -> bool {
    [transient, OBSERVATION_PHASES.contains(&phase)]
        .into_iter()
        .all(|observed| observed)
}

fn guard_failure_transient_state(
    root: &Path,
    h: &mut dyn GuardAdapter,
    notify: bool,
    mut s: J,
    phase: String,
    message: String,
) -> J {
    let count = model::u64_field(&s, "transient_errors").unwrap_or(0) + 1;
    s["transient_errors"] = json!(count);
    s["last_transient_error"] = json!(message);
    if count < TRANSIENT_LIMIT {
        let _ = save(root, &mut s);
        return s;
    }
    guard_failure_final(root, h, notify, s, phase, &message)
}

fn guard_failure_final(
    root: &Path,
    h: &mut dyn GuardAdapter,
    notify: bool,
    mut s: J,
    phase: String,
    message: &str,
) -> J {
    s["error"] = json!(message);
    s["phase_before_error"] = json!(phase);
    s["phase"] = json!("needs_attention");
    model::remove(&mut s, "transient_errors");
    let _ = save(root, &mut s);
    guard_failure_notify(root, h, notify, s, message)
}

fn guard_failure_notify(
    root: &Path,
    h: &mut dyn GuardAdapter,
    notify: bool,
    s: J,
    message: &str,
) -> J {
    if notify {
        h.set_state(&s);
        let _ = h.call(&[
            "notification".to_string(),
            "show".to_string(),
            "Herdr flow needs attention".to_string(),
            "--body".to_string(),
            format!("{}: {}", root.display(), message),
            "--sound".to_string(),
            "request".to_string(),
        ]);
    }
    s
}

pub fn guarded_once(root: &Path, h: &mut dyn GuardAdapter, notify: bool) -> J {
    once(root, h).map_or_else(
        |error| guarded_once_error(root, h, notify, error),
        |s| guarded_once_ok(root, s),
    )
}

fn guarded_once_ok(root: &Path, mut s: J) -> J {
    if model::get(&s, "transient_errors").is_some() {
        model::remove(&mut s, "transient_errors");
        model::remove(&mut s, "last_transient_error");
        let _ = save(root, &mut s);
    }
    s
}

fn guarded_once_error(root: &Path, h: &mut dyn GuardAdapter, notify: bool, error: FlowError) -> J {
    if error.kind == ErrorKind::AgentBusy {
        return read_state_or_empty(root);
    }
    guard_failure(root, h, notify, &error)
}

pub fn watch(root: &Path, ready_fd: Option<std::os::fd::RawFd>, one: bool) -> Result<()> {
    let root = util::resolve_path(root);
    watch_service::run(
        &root,
        || {
            let _lock = match locked(&root) {
                Ok(lock) => lock,
                Err(error) if error.kind == ErrorKind::StateBusy => {
                    if one {
                        return Err(error);
                    }
                    return Ok(false);
                }
                Err(error) => return Err(error),
            };
            let state = read_state(&root)?;
            if model::string(&state, "phase").as_deref() == Some("released") {
                return Ok(true);
            }
            let mut h = Herdr::new(&state)?;
            let s = guarded_once(&root, &mut h, true);
            if one {
                println!(
                    "{}",
                    serde_json::to_string(&s)
                        .map_err(|error| FlowError::new(ErrorKind::Json, error.to_string()))?
                );
            }
            Ok(false)
        },
        || validate_watch(&root),
        10,
        ready_fd,
        one,
    )
}

fn bind_runtime(
    s: &mut J,
    root: &Path,
    app_socket: Option<&str>,
    codex_binary: Option<&str>,
) -> Result<J> {
    let daemon = bind_daemon()?;
    bind_runtime_daemon(s, root, app_socket, codex_binary, daemon)
}

fn bind_daemon() -> Result<J> {
    let raw = command(
        &[
            "codex".to_string(),
            "app-server".to_string(),
            "daemon".to_string(),
            "version".to_string(),
        ],
        None,
        Duration::from_secs(15),
        false,
    )?;
    serde_json::from_slice(&raw)
        .map_err(|error| guard_error(format!("invalid app-server daemon response: {error}")))
}

fn bind_runtime_daemon(
    s: &mut J,
    root: &Path,
    app_socket: Option<&str>,
    codex_binary: Option<&str>,
    daemon: J,
) -> Result<J> {
    if model::string(&daemon, "status").as_deref() != Some("running") {
        return Err(guard_error(
            "a running local app-server is required; no automatic server startup",
        ));
    }
    bind_runtime_paths(s, root, app_socket, codex_binary, &daemon)
}

fn bind_runtime_paths(
    s: &mut J,
    root: &Path,
    app_socket: Option<&str>,
    codex_binary: Option<&str>,
    daemon: &J,
) -> Result<J> {
    let endpoint = app_socket
        .map(ToString::to_string)
        .or_else(|| model::string(daemon, "socketPath"))
        .unwrap_or_default();
    let binary = codex_binary
        .map(ToString::to_string)
        .or_else(|| model::string(daemon, "managedCodexPath"))
        .unwrap_or_default();
    if !bind_paths_ok(&endpoint, &binary) {
        return Err(guard_error(
            "explicit absolute app-server socket and Codex binary required",
        ));
    }
    bind_runtime_version(s, root, &endpoint, &binary, daemon)
}

fn bind_paths_ok(endpoint: &str, binary: &str) -> bool {
    [
        !endpoint.is_empty(),
        !binary.is_empty(),
        Path::new(endpoint).is_absolute(),
        Path::new(binary).is_absolute(),
    ]
    .into_iter()
    .all(|ok| ok)
}

fn bind_runtime_version(
    s: &mut J,
    root: &Path,
    endpoint: &str,
    binary: &str,
    daemon: &J,
) -> Result<J> {
    let actual_version = bind_binary_version(binary)?;
    bind_runtime_version_check(s, root, endpoint, binary, daemon, actual_version)
}

fn bind_binary_version(binary: &str) -> Result<String> {
    let raw = command(
        &[binary.to_string(), "--version".to_string()],
        None,
        Duration::from_secs(15),
        false,
    )?;
    Ok(String::from_utf8_lossy(&raw)
        .split_whitespace()
        .last()
        .unwrap_or_default()
        .to_string())
}

fn bind_runtime_version_check(
    s: &mut J,
    root: &Path,
    endpoint: &str,
    binary: &str,
    daemon: &J,
    actual_version: String,
) -> Result<J> {
    if Some(actual_version.as_str()) != model::string(daemon, "appServerVersion").as_deref() {
        return Err(guard_error(
            "TUI binary must match the running app-server version",
        ));
    }
    bind_runtime_register(s, root, endpoint, binary, actual_version)
}

fn bind_runtime_register(
    s: &mut J,
    root: &Path,
    endpoint: &str,
    binary: &str,
    actual_version: String,
) -> Result<J> {
    s["version"] = json!(2);
    s["state_dir"] = json!(util::resolve_path(root).display().to_string());
    s["app_server_socket"] = json!(endpoint);
    s["codex_binary"] = json!(util::resolve_path(Path::new(binary)).display().to_string());
    s["app_server_version"] = json!(actual_version);
    let marker = bind_marker(root)?;
    bind_runtime_marker(s, marker)
}

fn bind_marker(root: &Path) -> Result<J> {
    util::read_json(&util::resolve_path(root).join("tui.json"))
}

fn bind_runtime_marker(s: &mut J, marker: J) -> Result<J> {
    if bind_marker_bad(s, &marker) {
        return Err(guard_error(
            "registration requires a ready native TUI bridge for this exact thread",
        ));
    }
    s["bridge_nonce"] = marker.get("nonce").cloned().unwrap_or(Value::Null);
    bind_runtime_connect(s, &marker)
}

fn bind_marker_bad(s: &J, marker: &J) -> bool {
    [
        model::string(marker, "status").as_deref() != Some("ready"),
        model::string(marker, "session") != model::string(s, "session"),
        model::string(marker, "server_socket") != model::string(s, "app_server_socket"),
        !model::truthy(model::get(marker, "nonce")),
    ]
    .into_iter()
    .any(|bad| bad)
}

fn bind_runtime_connect(s: &mut J, marker: &J) -> Result<J> {
    let mut h = Herdr::new(s)?;
    bind_runtime_processes(s, &mut h, marker)
}

fn bind_runtime_processes(s: &mut J, h: &mut Herdr, marker: &J) -> Result<J> {
    let processes = h.processes()?;
    bind_runtime_identity(s, h, marker, &processes)
}

fn bind_runtime_identity(s: &mut J, h: &mut Herdr, marker: &J, processes: &J) -> Result<J> {
    s["shell_pid"] = processes.get("shell_pid").cloned().unwrap_or(Value::Null);
    let pid = model::u64_field(marker, "pid").unwrap_or(0) as u32;
    s["tui_process"] = util::process_identity(pid)?;
    bind_runtime_verify(s, h)
}

fn bind_runtime_verify(s: &mut J, h: &mut Herdr) -> Result<J> {
    h.set_state(s);
    let session = model::string(s, "session").unwrap_or_default();
    bind_runtime_verify_tui(s, h, &session)
}

fn bind_runtime_verify_tui(s: &mut J, h: &mut Herdr, session: &str) -> Result<J> {
    h.verify_tui(session)?;
    bind_runtime_agent(s, h)
}

fn bind_runtime_agent(s: &mut J, h: &mut Herdr) -> Result<J> {
    h.agent()?;
    bind_runtime_snapshot(s, h)
}

fn bind_runtime_snapshot(s: &mut J, h: &mut Herdr) -> Result<J> {
    h.snapshot(None)?;
    Ok(s.clone())
}

struct InitRequest<'a> {
    root: &'a Path,
    pane: &'a str,
    session: &'a str,
    transcript_path: &'a Path,
    task: &'a str,
    epoch: i64,
    role: &'a str,
    sessions_dir: &'a Path,
    app_socket: Option<&'a str>,
    codex_binary: Option<&'a str>,
}

#[allow(clippy::too_many_arguments)]
pub fn init(
    root: &Path,
    pane: &str,
    session: &str,
    transcript_path: &Path,
    task: &str,
    epoch: i64,
    role: &str,
    sessions_dir: &Path,
    app_socket: Option<&str>,
    codex_binary: Option<&str>,
) -> Result<J> {
    let request = InitRequest {
        root,
        pane,
        session,
        transcript_path,
        task,
        epoch,
        role,
        sessions_dir,
        app_socket,
        codex_binary,
    };
    init_session(&request)?;
    init_transcript(&request)
}

fn init_session(request: &InitRequest<'_>) -> Result<()> {
    let session_ok = regex::Regex::new(r"^[0-9a-f-]{36}$")
        .expect("static session regex")
        .is_match(request.session);
    [session_ok, request.epoch >= 0]
        .into_iter()
        .all(|valid| valid)
        .then_some(())
        .ok_or_else(|| guard_error("invalid session or epoch"))
}

fn init_transcript(request: &InitRequest<'_>) -> Result<J> {
    let data = transcript_state(request.transcript_path, None)?;
    init_transcript_session(request, data)
}

fn init_transcript_session(request: &InitRequest<'_>, data: J) -> Result<J> {
    if model::string(&data, "session").as_deref() != Some(request.session) {
        return Err(guard_error("transcript session mismatch"));
    }
    init_root(request, data)
}

fn init_root(request: &InitRequest<'_>, data: J) -> Result<J> {
    let root = util::resolve_path(request.root);
    if root.join("state.json").exists() {
        return Err(guard_error(
            "state exists; use it or choose a new state directory",
        ));
    }
    init_checkout(request, data, root)
}

fn init_checkout(request: &InitRequest<'_>, data: J, root: PathBuf) -> Result<J> {
    let cwd = model::string(&data, "cwd").unwrap_or_default();
    if util::inside_checkout(&root, Path::new(&cwd)) {
        return Err(guard_error("keep guard state outside the checkout"));
    }
    init_agent(request, root, cwd)
}

fn init_agent(request: &InitRequest<'_>, root: PathBuf, cwd: String) -> Result<J> {
    let raw = init_agent_raw(request.pane)?;
    init_agent_response(request, root, cwd, raw)
}

fn init_agent_raw(pane: &str) -> Result<Vec<u8>> {
    command(
        &[
            "herdr".to_string(),
            "agent".to_string(),
            "get".to_string(),
            pane.to_string(),
        ],
        None,
        Duration::from_secs(30),
        true,
    )
}

fn init_agent_response(
    request: &InitRequest<'_>,
    root: PathBuf,
    cwd: String,
    raw: Vec<u8>,
) -> Result<J> {
    let response: J = serde_json::from_slice(&raw)?;
    init_agent_parse(request, root, cwd, response)
}

fn init_agent_parse(
    request: &InitRequest<'_>,
    root: PathBuf,
    cwd: String,
    response: J,
) -> Result<J> {
    let agent = model::dig(&response, &["result", "agent"])
        .cloned()
        .ok_or_else(|| guard_error("Herdr agent response is incomplete"))?;
    init_state(request, root, cwd, agent)
}

fn init_state(request: &InitRequest<'_>, root: PathBuf, cwd: String, agent: J) -> Result<J> {
    let socket = std::env::var("HERDR_SOCKET_PATH").unwrap_or_default();
    let s = json!({
        "version": 1,
        "task": request.task,
        "epoch": request.epoch,
        "role": request.role,
        "pane": request.pane,
        "terminal": agent.get("terminal_id").cloned().unwrap_or(Value::Null),
        "session": request.session,
        "transcript": util::resolve_path(request.transcript_path).display().to_string(),
        "sessions_dir": util::resolve_path(request.sessions_dir).display().to_string(),
        "cwd": cwd,
        "socket": socket,
        "phase": "monitoring",
        "warn_tokens": 80000,
        "rotate_tokens": 120000,
        "warn_ratio": 0.60,
        "rotate_ratio": 0.75,
        "auto_rotate": false
    });
    if !model::truthy(model::get(&s, "socket")) {
        return Err(guard_error("explicit Herdr socket identity required"));
    }
    init_bind(request, s, root)
}

fn init_bind(request: &InitRequest<'_>, mut s: J, root: PathBuf) -> Result<J> {
    s = bind_runtime(&mut s, &root, request.app_socket, request.codex_binary)?;
    init_register(request, s, root)
}

fn init_register(request: &InitRequest<'_>, s: J, root: PathBuf) -> Result<J> {
    let registry = util::xdg_state_home().join("herdr-issue-flow/pane-guards");
    std::fs::create_dir_all(&registry)?;
    init_binding(request, s, root, registry)
}

fn init_binding(
    request: &InitRequest<'_>,
    mut s: J,
    root: PathBuf,
    registry: PathBuf,
) -> Result<J> {
    let binding_path = init_binding_path(&registry, &s, request.pane);
    let registration = json!({
        "state_dir": root.display().to_string(),
        "terminal": s.get("terminal").cloned().unwrap_or(Value::Null)
    });
    init_write_binding(&binding_path, &registration)?;
    s["binding"] = json!(binding_path.display().to_string());
    init_save(root, s)
}

fn init_binding_path(registry: &Path, s: &J, pane: &str) -> PathBuf {
    registry.join(format!(
        "{}.json",
        digest(format!("{}:{pane}", model::string(s, "socket").unwrap_or_default()).as_bytes())
    ))
}

fn init_write_binding(binding_path: &Path, registration: &J) -> Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true).mode(0o600);
    let file = init_open_binding(&mut options, binding_path)?;
    init_write_file(file, registration)
}

fn init_open_binding(options: &mut OpenOptions, path: &Path) -> Result<File> {
    options.open(path).map_err(init_binding_io_error)
}

fn init_binding_io_error(error: std::io::Error) -> FlowError {
    if error.kind() == std::io::ErrorKind::AlreadyExists {
        return guard_error(
            "pane already registered; release its existing guard before registering again",
        );
    }
    error.into()
}

fn init_write_file(mut file: File, registration: &J) -> Result<()> {
    let mut text = serde_json::to_string(registration)?;
    init_write_text(&mut file, &mut text)
}

fn init_write_text(file: &mut File, text: &mut String) -> Result<()> {
    text.push('\n');
    init_write_bytes(file, text.as_bytes())
}

fn init_write_bytes(file: &mut File, bytes: &[u8]) -> Result<()> {
    file.write_all(bytes)?;
    file.sync_all().map_err(FlowError::from)
}

fn init_save(root: PathBuf, mut s: J) -> Result<J> {
    save(&root, &mut s)?;
    Ok(s)
}

struct CmdInitInputs<'a> {
    pane: &'a str,
    session: &'a str,
    transcript: &'a Path,
    task: &'a str,
}

fn cmd_init(args: &Args, root: &Path) -> Result<J> {
    let inputs = cmd_init_inputs(args)?;
    cmd_init_run(args, root, &inputs)
}

fn cmd_init_inputs(args: &Args) -> Result<CmdInitInputs<'_>> {
    let Some(pane) = args.pane.as_deref() else {
        return Err(guard_error(
            "init requires pane, session, transcript and task",
        ));
    };
    let Some(session) = args.session.as_deref() else {
        return Err(guard_error(
            "init requires pane, session, transcript and task",
        ));
    };
    let Some(transcript) = args.transcript.as_deref() else {
        return Err(guard_error(
            "init requires pane, session, transcript and task",
        ));
    };
    let Some(task) = args.task.as_deref() else {
        return Err(guard_error(
            "init requires pane, session, transcript and task",
        ));
    };
    cmd_init_thresholds(args)?;
    Ok(CmdInitInputs {
        pane,
        session,
        transcript,
        task,
    })
}

fn cmd_init_thresholds(args: &Args) -> Result<()> {
    let tokens_ok = [
        0.0 < args.warn_tokens as f64,
        (args.warn_tokens as f64) < args.rotate_tokens as f64,
    ]
    .into_iter()
    .all(|valid| valid);
    let ratios_ok = [
        0.0 < args.warn_ratio,
        args.warn_ratio < args.rotate_ratio,
        args.rotate_ratio < 1.0,
    ]
    .into_iter()
    .all(|valid| valid);
    [tokens_ok, ratios_ok]
        .into_iter()
        .all(|valid| valid)
        .then_some(())
        .ok_or_else(|| guard_error("require positive ordered token thresholds and ratios"))
}

fn cmd_init_run(args: &Args, root: &Path, inputs: &CmdInitInputs<'_>) -> Result<J> {
    let s = cmd_init_create(args, root, inputs)?;
    cmd_init_save(args, root, s)
}

fn cmd_init_create(args: &Args, root: &Path, inputs: &CmdInitInputs<'_>) -> Result<J> {
    init(
        root,
        inputs.pane,
        inputs.session,
        inputs.transcript,
        inputs.task,
        args.epoch,
        &args.role,
        &args.sessions_dir,
        args.app_server_socket.as_deref(),
        args.codex_binary.as_deref(),
    )
}

fn cmd_init_save(args: &Args, root: &Path, mut s: J) -> Result<J> {
    cmd_init_fields(args, &mut s);
    save(root, &mut s)?;
    cmd_init_review(args, root, s)
}

fn cmd_init_fields(args: &Args, s: &mut J) {
    s["warn_tokens"] = json!(args.warn_tokens);
    s["rotate_tokens"] = json!(args.rotate_tokens);
    s["warn_ratio"] = json!(args.warn_ratio);
    s["rotate_ratio"] = json!(args.rotate_ratio);
    s["advisor_enabled"] = json!(!args.no_advisor);
}

fn cmd_init_review(args: &Args, root: &Path, s: J) -> Result<J> {
    if args.auto_review {
        return cmd_init_auto_review(args, root, s);
    }
    Ok(s)
}

fn cmd_init_auto_review(args: &Args, root: &Path, s: J) -> Result<J> {
    let mut h = Herdr::new(&s)?;
    configure_review(root, args.handoff.as_deref(), &mut h, false)
}

fn cmd_migrate(root: &Path, args: &Args) -> Result<J> {
    let s = read_state(root)?;
    cmd_migrate_settled(root, args, s)
}

fn cmd_migrate_settled(root: &Path, args: &Args, s: J) -> Result<J> {
    cmd_migrate_check(&s)?;
    cmd_migrate_lock(root, args, s)
}

fn cmd_migrate_check(s: &J) -> Result<()> {
    [
        matches!(
            model::string(s, "phase").as_deref(),
            Some("monitoring" | "parked")
        ),
        !external_delivery_pending(s),
        !model::bool_or(s, "auto_rotate", false),
    ]
    .into_iter()
    .all(|settled| settled)
    .then_some(())
    .ok_or_else(|| {
        guard_error("migration requires a settled, disabled guard without pending delivery")
    })
}

fn cmd_migrate_lock(root: &Path, args: &Args, s: J) -> Result<J> {
    let watcher = cmd_migrate_watcher(root)?;
    cmd_migrate_watcher_check(root, args, s, watcher)
}

fn cmd_migrate_watcher(root: &Path) -> Result<Option<util::FileLock>> {
    let watcher_path = util::resolve_path(root).join("watch.lock");
    util::FileLock::acquire(&watcher_path, false)
}

fn cmd_migrate_watcher_check(
    root: &Path,
    args: &Args,
    s: J,
    watcher: Option<util::FileLock>,
) -> Result<J> {
    if watcher.is_none() {
        return Err(guard_error(
            "stop the existing watcher before migrating its state",
        ));
    }
    cmd_migrate_bind(root, args, s)
}

fn cmd_migrate_bind(root: &Path, args: &Args, mut s: J) -> Result<J> {
    let old = s.clone();
    s = bind_runtime(
        &mut s,
        root,
        args.app_server_socket.as_deref(),
        args.codex_binary.as_deref(),
    )?;
    cmd_migrate_history(root, s, old)
}

fn cmd_migrate_history(root: &Path, s: J, old: J) -> Result<J> {
    let history = util::resolve_path(root)
        .join("history")
        .join(format!("before-migration-{}.json", util::now_ns()));
    cmd_migrate_history_write(&history, &old)?;
    cmd_migrate_save(root, s)
}

fn cmd_migrate_history_write(history: &Path, old: &J) -> Result<()> {
    util::atomic_write_json(history, old)
}

fn cmd_migrate_save(root: &Path, mut s: J) -> Result<J> {
    save(root, &mut s)?;
    Ok(s)
}

fn cmd_release(root: &Path) -> Result<J> {
    let s = read_state(root)?;
    cmd_release_delivery(root, s)
}

fn cmd_release_delivery(root: &Path, s: J) -> Result<J> {
    if external_delivery_pending(&s) {
        return Err(guard_error(
            "resolve wait-queue delivery before releasing receiver",
        ));
    }
    cmd_release_phase(root, s)
}

fn cmd_release_phase(root: &Path, s: J) -> Result<J> {
    if !matches!(
        model::string(&s, "phase").as_deref(),
        Some("monitoring" | "ready" | "parked" | "released")
    ) {
        return Err(guard_error(
            "resolve in-flight handoff before releasing pane registration",
        ));
    }
    cmd_release_save(root, s)
}

fn cmd_release_save(root: &Path, mut s: J) -> Result<J> {
    s["auto_rotate"] = json!(false);
    s["phase"] = json!("released");
    save(root, &mut s)?;
    cmd_release_binding(root, s)
}

fn cmd_release_binding(root: &Path, s: J) -> Result<J> {
    let Some(binding) = model::string(&s, "binding") else {
        return Ok(s);
    };
    cmd_release_unbind(root, &binding);
    Ok(s)
}

fn cmd_release_unbind(root: &Path, binding: &str) {
    let path = Path::new(binding);
    if !path.exists() {
        return;
    }
    cmd_release_remove_owned(root, path);
}

fn cmd_release_remove_owned(root: &Path, path: &Path) {
    let Ok(registration) = util::read_json(path) else {
        return;
    };
    cmd_release_remove_if_owned(root, &registration, path);
}

fn cmd_release_remove_if_owned(root: &Path, registration: &J, path: &Path) {
    if model::string(registration, "state_dir")
        == Some(util::resolve_path(root).display().to_string())
    {
        let _ = std::fs::remove_file(path);
    }
}

fn cmd_toggle(root: &Path, enable: bool) -> Result<J> {
    let s = read_state(root)?;
    cmd_toggle_branch(root, enable, s)
}

fn cmd_toggle_branch(root: &Path, enable: bool, s: J) -> Result<J> {
    if enable {
        return cmd_toggle_enable(root, s);
    }
    cmd_toggle_save(root, s, enable)
}

fn cmd_toggle_enable(root: &Path, s: J) -> Result<J> {
    validate_watch(root)?;
    cmd_toggle_attention(root, s)
}

fn cmd_toggle_attention(root: &Path, s: J) -> Result<J> {
    if model::string(&s, "phase").as_deref() == Some("needs_attention") {
        return Err(guard_error(
            "inspect and reconcile before enabling automation",
        ));
    }
    cmd_toggle_background(root, s)
}

fn cmd_toggle_background(root: &Path, s: J) -> Result<J> {
    let _ = watch_service::background(
        root,
        &[
            "guard".to_string(),
            "watch".to_string(),
            "--state-dir".to_string(),
            util::resolve_path(root).display().to_string(),
        ],
        || validate_watch(root),
        Duration::from_secs(10),
    )?;
    cmd_toggle_deadline(root, s)
}

fn cmd_toggle_deadline(root: &Path, mut s: J) -> Result<J> {
    if matches!(
        model::string(&s, "phase").as_deref(),
        Some("awaiting_shell" | "awaiting_tui")
    ) {
        s["transition_deadline"] = json!(util::now() + 120.0);
    }
    cmd_toggle_save(root, s, true)
}

fn cmd_toggle_save(root: &Path, mut s: J, enable: bool) -> Result<J> {
    s["auto_rotate"] = json!(enable);
    save(root, &mut s)?;
    Ok(s)
}

#[derive(Default)]
struct Args {
    action: String,
    state_dir: PathBuf,
    pane: Option<String>,
    session: Option<String>,
    transcript: Option<PathBuf>,
    task: Option<String>,
    role: String,
    epoch: i64,
    sessions_dir: PathBuf,
    warn_tokens: i64,
    rotate_tokens: i64,
    warn_ratio: f64,
    rotate_ratio: f64,
    handoff: Option<PathBuf>,
    once: bool,
    background: bool,
    ready_fd: Option<std::os::fd::RawFd>,
    app_server_socket: Option<String>,
    codex_binary: Option<String>,
    no_advisor: bool,
    auto_review: bool,
    wait: f64,
    shell: bool,
}

fn parse_value_at(args: &[String], index: usize) -> Result<String> {
    args.get(index + 1)
        .cloned()
        .ok_or_else(|| FlowError::usage("missing option value"))
}

fn parse_text_option(args: &[String], index: usize) -> Result<String> {
    parse_value_at(args, index)
}

fn parse_path_option(args: &[String], index: usize) -> Result<PathBuf> {
    let value = parse_value_at(args, index)?;
    Ok(PathBuf::from(value))
}

fn parse_i64_option(args: &[String], index: usize, message: &'static str) -> Result<i64> {
    let value = parse_value_at(args, index)?;
    value.parse().map_err(|_| FlowError::usage(message))
}

fn parse_f64_option(args: &[String], index: usize, message: &'static str) -> Result<f64> {
    let value = parse_value_at(args, index)?;
    value.parse().map_err(|_| FlowError::usage(message))
}

fn parse_fd_option(args: &[String], index: usize) -> Result<i32> {
    let value = parse_value_at(args, index)?;
    value
        .parse()
        .map_err(|_| FlowError::usage("invalid --ready-fd"))
}

fn parse_opt_state_dir(args: &[String], index: usize, parsed: &mut Args) -> Result<usize> {
    parsed.state_dir = parse_path_option(args, index)?;
    Ok(index + 2)
}

fn parse_opt_pane(args: &[String], index: usize, parsed: &mut Args) -> Result<usize> {
    parsed.pane = Some(parse_text_option(args, index)?);
    Ok(index + 2)
}

fn parse_opt_session(args: &[String], index: usize, parsed: &mut Args) -> Result<usize> {
    parsed.session = Some(parse_text_option(args, index)?);
    Ok(index + 2)
}

fn parse_opt_transcript(args: &[String], index: usize, parsed: &mut Args) -> Result<usize> {
    parsed.transcript = Some(parse_path_option(args, index)?);
    Ok(index + 2)
}

fn parse_opt_task(args: &[String], index: usize, parsed: &mut Args) -> Result<usize> {
    parsed.task = Some(parse_text_option(args, index)?);
    Ok(index + 2)
}

fn parse_opt_role(args: &[String], index: usize, parsed: &mut Args) -> Result<usize> {
    parsed.role = parse_text_option(args, index)?;
    Ok(index + 2)
}

fn parse_opt_epoch(args: &[String], index: usize, parsed: &mut Args) -> Result<usize> {
    parsed.epoch = parse_i64_option(args, index, "invalid --epoch")?;
    Ok(index + 2)
}

fn parse_opt_sessions_dir(args: &[String], index: usize, parsed: &mut Args) -> Result<usize> {
    parsed.sessions_dir = parse_path_option(args, index)?;
    Ok(index + 2)
}

fn parse_opt_warn_tokens(args: &[String], index: usize, parsed: &mut Args) -> Result<usize> {
    parsed.warn_tokens = parse_i64_option(args, index, "invalid --warn-tokens")?;
    Ok(index + 2)
}

fn parse_opt_rotate_tokens(args: &[String], index: usize, parsed: &mut Args) -> Result<usize> {
    parsed.rotate_tokens = parse_i64_option(args, index, "invalid --rotate-tokens")?;
    Ok(index + 2)
}

fn parse_opt_warn_ratio(args: &[String], index: usize, parsed: &mut Args) -> Result<usize> {
    parsed.warn_ratio = parse_f64_option(args, index, "invalid --warn-ratio")?;
    Ok(index + 2)
}

fn parse_opt_rotate_ratio(args: &[String], index: usize, parsed: &mut Args) -> Result<usize> {
    parsed.rotate_ratio = parse_f64_option(args, index, "invalid --rotate-ratio")?;
    Ok(index + 2)
}

fn parse_opt_handoff(args: &[String], index: usize, parsed: &mut Args) -> Result<usize> {
    parsed.handoff = Some(parse_path_option(args, index)?);
    Ok(index + 2)
}

fn parse_opt_ready_fd(args: &[String], index: usize, parsed: &mut Args) -> Result<usize> {
    parsed.ready_fd = Some(parse_fd_option(args, index)?);
    Ok(index + 2)
}

fn parse_opt_app_server_socket(args: &[String], index: usize, parsed: &mut Args) -> Result<usize> {
    parsed.app_server_socket = Some(parse_text_option(args, index)?);
    Ok(index + 2)
}

fn parse_opt_codex_binary(args: &[String], index: usize, parsed: &mut Args) -> Result<usize> {
    parsed.codex_binary = Some(parse_text_option(args, index)?);
    Ok(index + 2)
}

fn parse_opt_wait(args: &[String], index: usize, parsed: &mut Args) -> Result<usize> {
    parsed.wait = parse_f64_option(args, index, "invalid --wait")?;
    Ok(index + 2)
}

fn parse_flag_once(_args: &[String], index: usize, parsed: &mut Args) -> Result<usize> {
    parsed.once = true;
    Ok(index + 1)
}

fn parse_flag_background(_args: &[String], index: usize, parsed: &mut Args) -> Result<usize> {
    parsed.background = true;
    Ok(index + 1)
}

fn parse_flag_no_advisor(_args: &[String], index: usize, parsed: &mut Args) -> Result<usize> {
    parsed.no_advisor = true;
    Ok(index + 1)
}

fn parse_flag_auto_review(_args: &[String], index: usize, parsed: &mut Args) -> Result<usize> {
    parsed.auto_review = true;
    Ok(index + 1)
}

fn parse_flag_shell(_args: &[String], index: usize, parsed: &mut Args) -> Result<usize> {
    parsed.shell = true;
    Ok(index + 1)
}

fn parse_help(_args: &[String], _index: usize, _parsed: &mut Args) -> Result<usize> {
    Err(FlowError::usage("help requested"))
}

type OptionParser = fn(&[String], usize, &mut Args) -> Result<usize>;

struct ParsedOption {
    name: &'static str,
    parse: OptionParser,
}

static OPTION_PARSERS: &[ParsedOption] = &[
    ParsedOption {
        name: "--state-dir",
        parse: parse_opt_state_dir,
    },
    ParsedOption {
        name: "--pane",
        parse: parse_opt_pane,
    },
    ParsedOption {
        name: "--session",
        parse: parse_opt_session,
    },
    ParsedOption {
        name: "--transcript",
        parse: parse_opt_transcript,
    },
    ParsedOption {
        name: "--task",
        parse: parse_opt_task,
    },
    ParsedOption {
        name: "--role",
        parse: parse_opt_role,
    },
    ParsedOption {
        name: "--epoch",
        parse: parse_opt_epoch,
    },
    ParsedOption {
        name: "--sessions-dir",
        parse: parse_opt_sessions_dir,
    },
    ParsedOption {
        name: "--warn-tokens",
        parse: parse_opt_warn_tokens,
    },
    ParsedOption {
        name: "--rotate-tokens",
        parse: parse_opt_rotate_tokens,
    },
    ParsedOption {
        name: "--warn-ratio",
        parse: parse_opt_warn_ratio,
    },
    ParsedOption {
        name: "--rotate-ratio",
        parse: parse_opt_rotate_ratio,
    },
    ParsedOption {
        name: "--handoff",
        parse: parse_opt_handoff,
    },
    ParsedOption {
        name: "--once",
        parse: parse_flag_once,
    },
    ParsedOption {
        name: "--background",
        parse: parse_flag_background,
    },
    ParsedOption {
        name: "--ready-fd",
        parse: parse_opt_ready_fd,
    },
    ParsedOption {
        name: "--app-server-socket",
        parse: parse_opt_app_server_socket,
    },
    ParsedOption {
        name: "--codex-binary",
        parse: parse_opt_codex_binary,
    },
    ParsedOption {
        name: "--no-advisor",
        parse: parse_flag_no_advisor,
    },
    ParsedOption {
        name: "--auto-review",
        parse: parse_flag_auto_review,
    },
    ParsedOption {
        name: "--wait",
        parse: parse_opt_wait,
    },
    ParsedOption {
        name: "--shell",
        parse: parse_flag_shell,
    },
    ParsedOption {
        name: "-h",
        parse: parse_help,
    },
    ParsedOption {
        name: "--help",
        parse: parse_help,
    },
];

const ACTIONS: &[&str] = &[
    "init",
    "migrate",
    "status",
    "watch",
    "checkpoint",
    "ack",
    "advise",
    "review-enable",
    "review-reconcile",
    "enable",
    "disable",
    "release",
    "reconcile",
];

fn parse_args(args: &[String]) -> Result<Args> {
    let mut parsed = parse_args_defaults();
    parse_args_loop(args, &mut parsed)?;
    parse_args_finish(parsed)
}

fn parse_args_defaults() -> Args {
    Args {
        role: "dev".to_string(),
        epoch: 1,
        sessions_dir: util::codex_home().join("sessions"),
        warn_tokens: 80000,
        rotate_tokens: 120000,
        warn_ratio: 0.60,
        rotate_ratio: 0.75,
        ..Default::default()
    }
}

fn parse_args_loop(args: &[String], parsed: &mut Args) -> Result<()> {
    parse_args_loop_from(args, 0, parsed)
}

fn parse_args_loop_from(args: &[String], index: usize, parsed: &mut Args) -> Result<()> {
    let Some(next) = parse_args_loop_next(args, index, parsed)? else {
        return Ok(());
    };
    parse_args_loop_from(args, next, parsed)
}

fn parse_args_loop_next(args: &[String], index: usize, parsed: &mut Args) -> Result<Option<usize>> {
    if index >= args.len() {
        return Ok(None);
    }
    parse_args_dispatch(args, index, parsed).map(Some)
}

fn parse_args_dispatch(args: &[String], index: usize, parsed: &mut Args) -> Result<usize> {
    let argument = args[index].as_str();
    let option = OPTION_PARSERS.iter().find(|option| option.name == argument);
    parse_args_selected(args, index, parsed, option)
}

fn parse_args_selected(
    args: &[String],
    index: usize,
    parsed: &mut Args,
    option: Option<&ParsedOption>,
) -> Result<usize> {
    if let Some(option) = option {
        return (option.parse)(args, index, parsed);
    }
    parse_args_fallback(args, index, parsed)
}

fn parse_args_fallback(args: &[String], index: usize, parsed: &mut Args) -> Result<usize> {
    if args[index].starts_with('-') {
        return parse_args_unknown(&args[index]);
    }
    parse_args_positional(args, index, parsed)
}

fn parse_args_unknown(argument: &str) -> Result<usize> {
    Err(FlowError::usage(format!(
        "unrecognized argument: {argument}"
    )))
}

fn parse_args_positional(args: &[String], index: usize, parsed: &mut Args) -> Result<usize> {
    if !parsed.action.is_empty() {
        return Err(FlowError::usage(format!(
            "unrecognized argument: {}",
            args[index]
        )));
    }
    parsed.action = args[index].clone();
    Ok(index + 1)
}

fn parse_args_finish(mut parsed: Args) -> Result<Args> {
    parse_args_validate(&mut parsed)?;
    Ok(parsed)
}

fn parse_args_validate(parsed: &mut Args) -> Result<()> {
    parse_args_action_required(parsed)?;
    parse_args_after_action(parsed)
}

fn parse_args_action_required(parsed: &Args) -> Result<()> {
    if parsed.action.is_empty() {
        return Err(FlowError::usage("an action is required"));
    }
    Ok(())
}

fn parse_args_after_action(parsed: &mut Args) -> Result<()> {
    parse_args_action_allowed(&parsed.action)?;
    parse_args_state_dir(parsed)
}

fn parse_args_action_allowed(action: &str) -> Result<()> {
    ACTIONS
        .contains(&action)
        .then_some(())
        .ok_or_else(|| FlowError::usage(format!("invalid action: {action}")))
}

fn parse_args_state_dir(parsed: &mut Args) -> Result<()> {
    if parsed.state_dir.as_os_str().is_empty() {
        return Err(FlowError::usage("--state-dir is required"));
    }
    parse_args_session_env(parsed)
}

fn parse_args_session_env(parsed: &mut Args) -> Result<()> {
    if parsed.session.is_none() {
        parsed.session = std::env::var("CODEX_THREAD_ID").ok();
    }
    parse_args_wait(parsed)
}

fn parse_args_wait(parsed: &Args) -> Result<()> {
    let valid = [
        (0.0..=60.0).contains(&parsed.wait),
        parse_args_wait_action_ok(parsed),
    ]
    .into_iter()
    .all(|valid| valid);
    if !valid {
        return Err(guard_error(
            "--wait requires advise and a duration from 0 to 60 seconds",
        ));
    }
    parse_args_no_advisor(parsed)
}

fn parse_args_wait_action_ok(parsed: &Args) -> bool {
    parsed.wait == 0.0 || parsed.action == "advise"
}

fn parse_args_no_advisor(parsed: &Args) -> Result<()> {
    if [parsed.no_advisor, parsed.action != "init"]
        .into_iter()
        .all(|invalid| invalid)
    {
        return Err(guard_error("--no-advisor is an init option"));
    }
    parse_args_auto_review(parsed)
}

fn parse_args_auto_review(parsed: &Args) -> Result<()> {
    if [parsed.auto_review, parsed.action != "init"]
        .into_iter()
        .all(|invalid| invalid)
    {
        return Err(guard_error(
            "--auto-review is an init option; existing guards use review-enable",
        ));
    }
    parse_args_background(parsed)
}

fn parse_args_background(parsed: &Args) -> Result<()> {
    let blocking = [parsed.action != "watch", parsed.once]
        .into_iter()
        .any(|blocked| blocked);
    if [parsed.background, blocking]
        .into_iter()
        .all(|invalid| invalid)
    {
        return Err(guard_error("--background requires watch without --once"));
    }
    parse_args_session_owner(parsed)
}

fn parse_args_session_owner(parsed: &Args) -> Result<()> {
    if !parse_args_owned_action(&parsed.action) {
        return Ok(());
    }
    parse_args_owned_env(parsed)
}

fn parse_args_owned_action(action: &str) -> bool {
    matches!(action, "checkpoint" | "ack" | "advise")
}

fn parse_args_owned_env(parsed: &Args) -> Result<()> {
    let env_session = std::env::var("CODEX_THREAD_ID").ok();
    let mismatch = [env_session.is_none(), parsed.session != env_session]
        .into_iter()
        .any(|mismatch| mismatch);
    if mismatch {
        return Err(guard_error(
            "checkpoint/ack/advise must be called by the identified Codex session itself",
        ));
    }
    Ok(())
}

fn print_status(root: &Path) -> Result<()> {
    let s = read_state(root)?;
    let output = cli_output(root, &s);
    cli_print_output(&output)
}

fn cli_error(error: FlowError) -> i32 {
    util::write_stderr(&format!("{}: {}\n", error.kind.type_name(), error));
    2
}

fn cli_pretty(value: &J) -> Result<String> {
    serde_json::to_string_pretty(value)
        .map_err(|error| FlowError::new(ErrorKind::Json, error.to_string()))
}

fn cli_print_output(output: &J) -> Result<()> {
    let text = cli_pretty(output)?;
    println!("{text}");
    Ok(())
}

fn cli_output(root: &Path, state: &J) -> J {
    let watcher = watch_service::health(&util::resolve_path(root));
    let mut output = state.clone();
    output["watcher"] = watcher.clone();
    output["diagnosis"] = diagnosis(state, &watcher);
    output["advice"] = advice_status(root, state)
        .unwrap_or_else(|error| json!({"status": "unavailable", "error": error.to_string()}));
    output["review_events"] = review_events(root, state)
        .unwrap_or_else(|error| json!({"status": "unavailable", "error": error.to_string()}));
    output
}

fn cli_help() -> i32 {
    println!(
        "Usage: herdr-flow guard ACTION --state-dir DIR [options]\nActions: init, migrate, status, watch, checkpoint, ack, advise, review-enable, review-reconcile, enable, disable, release, reconcile"
    );
    0
}

fn cli_parse_error(error: FlowError) -> i32 {
    if error.message == "help requested" {
        return cli_help();
    }
    cli_error(error)
}

pub fn cli(args: &[String]) -> i32 {
    parse_args(args).map_or_else(cli_parse_error, |parsed| cli_parsed(&parsed))
}

fn cli_parsed(parsed: &Args) -> i32 {
    let root = util::resolve_path(&parsed.state_dir);
    cli_action(parsed, root)
}

fn cli_action(parsed: &Args, root: PathBuf) -> i32 {
    if parsed.action == "watch" {
        return cli_watch(parsed, &root);
    }
    cli_locked(parsed, &root)
}

fn cli_watch(parsed: &Args, root: &Path) -> i32 {
    if parsed.background {
        return cli_watch_background(root);
    }
    cli_watch_foreground(parsed, root)
}

fn cli_watch_background(root: &Path) -> i32 {
    watch_service::background(
        root,
        &[
            "guard".to_string(),
            "watch".to_string(),
            "--state-dir".to_string(),
            root.display().to_string(),
        ],
        || validate_watch(root),
        Duration::from_secs(10),
    )
    .map_or_else(cli_error, |value| {
        println!("{value}");
        0
    })
}

fn cli_watch_foreground(parsed: &Args, root: &Path) -> i32 {
    watch(root, parsed.ready_fd, parsed.once).map_or_else(cli_error, |()| 0)
}

fn cli_locked(parsed: &Args, root: &Path) -> i32 {
    cli_run(parsed, root).map_or_else(cli_error, |state| cli_finish(root, state))
}

fn cli_finish(root: &Path, state: Option<J>) -> i32 {
    state.map_or(0, |state| cli_emit_state(root, &state))
}

fn cli_emit_state(root: &Path, state: &J) -> i32 {
    let output = cli_output(root, state);
    serde_json::to_string_pretty(&output).map_or_else(
        |error| cli_error(FlowError::new(ErrorKind::Json, error.to_string())),
        |text| {
            println!("{text}");
            0
        },
    )
}

fn cli_run(parsed: &Args, root: &Path) -> Result<Option<J>> {
    let _lock = locked(root)?;
    cli_dispatch(parsed, root)
}

type CliAction = fn(&Args, &Path) -> Result<Option<J>>;

static CLI_ACTIONS: &[(&str, CliAction)] = &[
    ("init", cli_action_init),
    ("migrate", cli_action_migrate),
    ("status", cli_action_status),
    ("checkpoint", cli_action_checkpoint),
    ("ack", cli_action_ack),
    ("advise", cli_action_advise),
    ("review-enable", cli_action_review_enable),
    ("review-reconcile", cli_action_review_reconcile),
    ("enable", cli_action_toggle),
    ("disable", cli_action_toggle),
    ("release", cli_action_release),
    ("reconcile", cli_action_reconcile),
];

fn cli_dispatch(parsed: &Args, root: &Path) -> Result<Option<J>> {
    CLI_ACTIONS
        .iter()
        .find(|entry| entry.0 == parsed.action.as_str())
        .map_or_else(|| Ok(None), |entry| (entry.1)(parsed, root))
}

fn cli_action_init(parsed: &Args, root: &Path) -> Result<Option<J>> {
    cmd_init(parsed, root).map(Some)
}

fn cli_action_migrate(parsed: &Args, root: &Path) -> Result<Option<J>> {
    cmd_migrate(root, parsed).map(Some)
}

fn cli_action_status(_parsed: &Args, root: &Path) -> Result<Option<J>> {
    print_status(root)?;
    Ok(None)
}

fn cli_handoff(parsed: &Args) -> Result<&Path> {
    parsed
        .handoff
        .as_deref()
        .ok_or_else(|| guard_error("--handoff required"))
}

fn cli_action_checkpoint(parsed: &Args, root: &Path) -> Result<Option<J>> {
    let handoff = cli_handoff(parsed)?;
    cli_checkpoint_run(parsed, root, handoff)
}

fn cli_checkpoint_run(parsed: &Args, root: &Path, handoff: &Path) -> Result<Option<J>> {
    checkpoint(root, handoff, parsed.session.as_deref().unwrap_or_default()).map(Some)
}

fn cli_action_ack(parsed: &Args, root: &Path) -> Result<Option<J>> {
    ack(root, parsed.session.as_deref().unwrap_or_default()).map(Some)
}

fn cli_action_advise(parsed: &Args, root: &Path) -> Result<Option<J>> {
    let handoff = cli_handoff(parsed)?;
    cli_advise_run(parsed, root, handoff)
}

fn cli_advise_run(parsed: &Args, root: &Path, handoff: &Path) -> Result<Option<J>> {
    let advice = advise(root, handoff, parsed.session.as_deref().unwrap_or_default())?;
    cli_advise_after(parsed, root, advice)
}

fn cli_advise_after(parsed: &Args, root: &Path, advice: J) -> Result<Option<J>> {
    if cli_advise_should_wait(parsed, &advice) {
        return cli_advise_wait(parsed, root);
    }
    cli_advise_print(&advice)
}

fn cli_advise_should_wait(parsed: &Args, advice: &J) -> bool {
    [
        parsed.wait != 0.0,
        model::string(advice, "status").as_deref() != Some("disabled"),
    ]
    .into_iter()
    .all(|wait| wait)
}

fn cli_advise_wait(parsed: &Args, root: &Path) -> Result<Option<J>> {
    let advice = wait_advice(root, parsed.wait)?;
    cli_advise_print(&advice)
}

fn cli_advise_print(advice: &J) -> Result<Option<J>> {
    let text = cli_pretty(advice)?;
    println!("{text}");
    Ok(None)
}

fn cli_action_review_enable(parsed: &Args, root: &Path) -> Result<Option<J>> {
    let state = read_state(root)?;
    cli_review_enable_run(parsed, root, &state)
}

fn cli_review_enable_run(parsed: &Args, root: &Path, state: &J) -> Result<Option<J>> {
    let mut h = Herdr::new(state)?;
    configure_review(root, parsed.handoff.as_deref(), &mut h, true).map(Some)
}

fn cli_action_review_reconcile(_parsed: &Args, root: &Path) -> Result<Option<J>> {
    let state = read_state(root)?;
    cli_review_reconcile_run(root, &state)
}

fn cli_review_reconcile_run(root: &Path, state: &J) -> Result<Option<J>> {
    let mut h = Herdr::new(state)?;
    reconcile_review(root, &mut h).map(Some)
}

fn cli_action_toggle(parsed: &Args, root: &Path) -> Result<Option<J>> {
    cmd_toggle(root, parsed.action == "enable").map(Some)
}

fn cli_action_release(_parsed: &Args, root: &Path) -> Result<Option<J>> {
    cmd_release(root).map(Some)
}

fn cli_action_reconcile(parsed: &Args, root: &Path) -> Result<Option<J>> {
    let state = read_state(root)?;
    cli_reconcile_run(parsed, root, &state)
}

fn cli_reconcile_run(parsed: &Args, root: &Path, state: &J) -> Result<Option<J>> {
    let mut h = Herdr::new(state)?;
    cli_reconcile_connected(parsed, root, &mut h)
}

fn cli_reconcile_connected(
    parsed: &Args,
    root: &Path,
    h: &mut dyn GuardAdapter,
) -> Result<Option<J>> {
    let s = reconcile(
        root,
        parsed.session.as_deref().unwrap_or_default(),
        h,
        parsed.shell,
    )?;
    cli_reconcile_background(root, s)
}

fn cli_reconcile_background(root: &Path, s: J) -> Result<Option<J>> {
    let _ = watch_service::background(
        root,
        &[
            "guard".to_string(),
            "watch".to_string(),
            "--state-dir".to_string(),
            root.display().to_string(),
        ],
        || validate_watch(root),
        Duration::from_secs(10),
    );
    Ok(Some(s))
}
