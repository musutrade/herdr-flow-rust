//! Durable waits and acknowledged, batched Herdr notifications. No model polling.

struct DeliveryResolution<'a> {
    delivery: &'a str,
    resolution: &'a str,
    reason: &'a str,
}

struct NoticeText<'a> {
    field: &'a str,
    title: &'a str,
    body: &'a str,
}

pub mod cli;
pub mod command;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

use crate::error::{ErrorKind, FlowError, Result};
use crate::guard;
use crate::herdr::ReceiverFactory;
use crate::logging;
use crate::model::{self, J};
use crate::progress;
use crate::util;
use crate::util::pyjson::{sha256_hex, sha256_json_sorted, uuid_hex};
use crate::watch_service;

pub use crate::guard::scope;
pub use command::run_command;

pub fn queue_error(message: impl Into<String>) -> FlowError {
    FlowError::queue(message)
}

pub fn load_queue(root: &Path) -> Result<J> {
    util::read_state_json(root, "queue.json")
}

pub fn locked(root: &Path, blocking: bool) -> Result<util::FileLock> {
    let root = util::state_dir(root, true)?;
    match util::FileLock::acquire(&root.join("queue.lock"), blocking)? {
        Some(lock) => Ok(lock),
        None => Err(FlowError::new(
            ErrorKind::StateBusy,
            "wait queue busy; retry after current poll",
        )),
    }
}

pub fn save(root: &Path, s: &mut J) -> Result<()> {
    util::ensure_private_dir(root, true)?;
    s["updated_at"] = json!(util::now());
    util::atomic_write_json(&root.join("queue.json"), s)
}

pub fn init(root: &Path, guard_dir: Option<&Path>, interval: u64) -> Result<J> {
    if !(10..=900).contains(&interval) {
        return Err(queue_error("poll interval must be 10..900 seconds"));
    }
    let root = util::state_dir(root, true)?;
    if root.join("queue.json").exists() {
        return Err(queue_error("queue already exists"));
    }
    let (receiver, stored_guard) = initial_receiver(&root, guard_dir)?;
    let mut s = json!({
        "version": 1,
        "id": uuid_hex(),
        "guard_dir": stored_guard,
        "receiver": receiver,
        "poll_seconds": interval,
        "notify": false,
        "stopped": false,
        "waits": {},
        "events": {},
        "deliveries": {},
        "active": null,
        "delivery_status": "disabled"
    });
    save(&root, &mut s)?;
    Ok(s)
}

fn initial_receiver(root: &Path, guard_dir: Option<&Path>) -> Result<(J, J)> {
    let mut receiver = Value::Null;
    let mut stored_guard: Value = Value::Null;
    if let Some(guard_dir) = guard_dir {
        let guard_dir = util::state_dir(guard_dir, false)?;
        let state = util::read_state_json(&guard_dir, "state.json")?;
        receiver = guard::scope(&state);
        let cwd = model::string(&receiver, "cwd")
            .ok_or_else(|| queue_error("receiver registration invalid"))?;
        if util::inside_checkout(root, Path::new(&cwd)) {
            return Err(queue_error(
                "keep queue state outside the receiver checkout",
            ));
        }
        stored_guard = json!(guard_dir.display().to_string());
    }
    Ok((receiver, stored_guard))
}

pub fn enable(root: &Path, s: &mut J, factory: &ReceiverFactory) -> Result<()> {
    let guard_path = enable_guard_path(s)?;
    enable_guarded(root, s, factory, &guard_path)
}

fn enable_guard_path(s: &J) -> Result<PathBuf> {
    let guard_dir = model::string(s, "guard_dir")
        .ok_or_else(|| queue_error("enabled delivery requires a live queue and receiver guard"))?;
    enable_guard_dir_checked(s, guard_dir)
}

fn enable_guard_dir_checked(s: &J, guard_dir: String) -> Result<PathBuf> {
    ensure_enable_queue(s)?;
    Ok(PathBuf::from(guard_dir))
}

fn ensure_enable_queue(s: &J) -> Result<()> {
    if model::bool_or(s, "stopped", false) {
        return Err(queue_error(
            "enabled delivery requires a live queue and receiver guard",
        ));
    }
    Ok(())
}

fn enable_guarded(
    root: &Path,
    s: &mut J,
    factory: &ReceiverFactory,
    guard_path: &Path,
) -> Result<()> {
    let _lock = guard::locked(guard_path)?;
    enable_locked(root, s, factory, guard_path)
}

fn enable_locked(
    root: &Path,
    s: &mut J,
    factory: &ReceiverFactory,
    guard_path: &Path,
) -> Result<()> {
    let g = util::read_state_json(guard_path, "state.json")?;
    enable_scope(root, s, factory, guard_path, &g)
}

fn enable_scope(
    root: &Path,
    s: &mut J,
    factory: &ReceiverFactory,
    guard_path: &Path,
    g: &J,
) -> Result<()> {
    ensure_receiver_scope(s, g)?;
    enable_ready(root, s, factory, guard_path, g)
}

fn ensure_receiver_scope(s: &J, g: &J) -> Result<()> {
    if guard::scope(g) != model::get(s, "receiver").cloned().unwrap_or(Value::Null) {
        return Err(queue_error(
            "receiver registration changed; explicit rebind required",
        ));
    }
    Ok(())
}

fn enable_ready(
    root: &Path,
    s: &mut J,
    factory: &ReceiverFactory,
    guard_path: &Path,
    g: &J,
) -> Result<()> {
    ensure_receiver_ready(g)?;
    enable_watcher(root, s, factory, guard_path, g)
}

fn ensure_receiver_ready(g: &J) -> Result<()> {
    match (
        model::string(g, "phase").as_deref() == Some("monitoring"),
        model::bool_or(g, "rotation_due", false),
        has_external_delivery(g),
    ) {
        (true, false, false) => Ok(()),
        _ => Err(queue_error(
            "receiver is not ready for notification enablement",
        )),
    }
}

fn has_external_delivery(g: &J) -> bool {
    model::get(g, "external_delivery")
        .map(|value| model::truthy(Some(value)))
        .unwrap_or(false)
}

fn enable_watcher(
    root: &Path,
    s: &mut J,
    factory: &ReceiverFactory,
    guard_path: &Path,
    g: &J,
) -> Result<()> {
    ensure_watcher_healthy(guard_path)?;
    enable_session(root, s, factory, g)
}

fn ensure_watcher_healthy(guard_path: &Path) -> Result<()> {
    if !model::bool_or(&watch_service::health(guard_path), "healthy", false) {
        return Err(queue_error(
            "receiver watcher is not healthy; restart and verify readiness",
        ));
    }
    Ok(())
}

fn enable_session(root: &Path, s: &mut J, factory: &ReceiverFactory, g: &J) -> Result<()> {
    let adapter = factory(g)?;
    enable_agent_session(root, s, g, adapter.as_ref())
}

fn enable_agent_session(
    root: &Path,
    s: &mut J,
    g: &J,
    adapter: &dyn crate::herdr::ReceiverAdapter,
) -> Result<()> {
    let agent = adapter.agent()?;
    enable_agent_checked(root, s, g, &agent)
}

fn enable_agent_checked(root: &Path, s: &mut J, g: &J, agent: &J) -> Result<()> {
    ensure_agent_session(g, agent)?;
    enable_commit(root, s)
}

fn ensure_agent_session(g: &J, agent: &J) -> Result<()> {
    if model::dig_str(agent, &["agent_session", "value"]) != model::string(g, "session") {
        return Err(queue_error(
            "live receiver session is not confirmed by app-server",
        ));
    }
    Ok(())
}

fn enable_commit(root: &Path, s: &mut J) -> Result<()> {
    s["notify"] = json!(true);
    s["delivery_status"] = json!("waiting_for_events");
    model::remove(s, "delivery_error");
    save(root, s)
}

fn validate_ci_wait(w: &J) -> Result<()> {
    let source_ok = model::string(w, "source")
        .map(|source| Path::new(&source).is_absolute())
        .unwrap_or(false);
    let hash_ok = model::string(w, "config_hash")
        .map(|value| model::is_sha256_hex(&value))
        .unwrap_or(false);
    if !source_ok || !hash_ok {
        return Err(queue_error(
            "CI wait requires absolute source directory and pinned config_hash",
        ));
    }
    Ok(())
}

fn validate_guard_wait(w: &J) -> Result<()> {
    let source_ok = model::string(w, "source")
        .map(|source| Path::new(&source).is_absolute())
        .unwrap_or(false);
    let identity = model::get(w, "guard_identity").filter(|value| value.is_object());
    let keys_ok = identity
        .and_then(Value::as_object)
        .map(|map| {
            let keys: BTreeSet<&str> = map.keys().map(String::as_str).collect();
            keys == BTreeSet::from(["task", "role", "pane", "terminal", "socket", "cwd"])
                && map
                    .values()
                    .all(|value| value.as_str().map(|text| !text.is_empty()).unwrap_or(false))
        })
        .unwrap_or(false);
    let task_ok =
        identity.and_then(|identity| model::string(identity, "task")) == model::string(w, "task");
    if !source_ok || !keys_ok || !task_ok {
        return Err(queue_error(
            "guard wait requires absolute source and pinned guard_identity matching task",
        ));
    }
    Ok(())
}

fn validate_wait_kind(w: &J) -> Result<()> {
    match model::string(w, "kind").as_deref() {
        Some("ci") => validate_ci_wait(w),
        Some("guard") => validate_guard_wait(w),
        Some("signal") => Ok(()),
        _ => Err(queue_error("kind must be ci, signal or guard")),
    }
}

const WAIT_REQUIRED_KEYS: &[&str] = &[
    "id",
    "task",
    "epoch",
    "owner",
    "reason",
    "condition",
    "kind",
    "timeout_seconds",
];

const WAIT_TEXT_KEYS: &[&str] = &["task", "owner", "reason", "condition"];

/// Validate the JSON object accepted by `queue add`.
pub fn validate_wait(w: &J) -> Result<()> {
    ensure_wait_required(w)?;
    validate_wait_fields(w)
}

fn ensure_wait_required(w: &J) -> Result<()> {
    if WAIT_REQUIRED_KEYS
        .iter()
        .any(|key| model::get(w, key).is_none())
    {
        return Err(queue_error(
            "wait requires id/task/epoch/owner/reason/condition/kind/timeout_seconds",
        ));
    }
    Ok(())
}

fn validate_wait_fields(w: &J) -> Result<()> {
    ensure_wait_id(w)?;
    validate_wait_epoch_stage(w)
}

fn validate_wait_epoch_stage(w: &J) -> Result<()> {
    ensure_wait_epoch(w)?;
    validate_wait_text_stage(w)
}

fn validate_wait_text_stage(w: &J) -> Result<()> {
    ensure_wait_texts(w)?;
    validate_wait_timeout_stage(w)
}

fn validate_wait_timeout_stage(w: &J) -> Result<()> {
    ensure_wait_timeout(w)?;
    validate_wait_kind_stage(w)
}

fn validate_wait_kind_stage(w: &J) -> Result<()> {
    validate_wait_kind(w)?;
    validate_wait_binding(w)
}

fn ensure_wait_id(w: &J) -> Result<()> {
    let id = model::string(w, "id").unwrap_or_default();
    let id_ok = regex::Regex::new(r"^[a-zA-Z0-9_-]{1,64}$")
        .expect("static wait ID regex")
        .is_match(&id);
    if !id_ok {
        return Err(queue_error("invalid wait ID"));
    }
    Ok(())
}

fn ensure_wait_epoch(w: &J) -> Result<()> {
    let epoch_ok = model::get(w, "epoch")
        .and_then(Value::as_i64)
        .map(|value| value >= 0)
        .unwrap_or(false);
    if !epoch_ok {
        return Err(queue_error("invalid epoch"));
    }
    Ok(())
}

fn ensure_wait_texts(w: &J) -> Result<()> {
    match WAIT_TEXT_KEYS.iter().find(|key| !valid_wait_text(w, key)) {
        Some(key) => Err(queue_error(format!("invalid {key}"))),
        _ => Ok(()),
    }
}

fn valid_wait_text(w: &J, key: &str) -> bool {
    model::string(w, key)
        .map(|value| !value.trim().is_empty() && value.chars().count() <= 500)
        .unwrap_or(false)
}

fn ensure_wait_timeout(w: &J) -> Result<()> {
    let duration_ok = model::number(w, "timeout_seconds")
        .map(|value| value.is_finite() && (1.0..=604_800.0).contains(&value))
        .unwrap_or(false);
    if !duration_ok {
        return Err(queue_error("timeout must be 1 second to 7 days"));
    }
    Ok(())
}

fn validate_wait_binding(w: &J) -> Result<()> {
    match (
        model::get(w, "assignment").is_some(),
        model::get(w, "candidate").is_some(),
    ) {
        (false, false) => Ok(()),
        _ => progress::binding(w).map(|_| ()),
    }
}

pub fn add(root: &Path, spec: &J) -> Result<J> {
    let mut s = load_queue(root)?;
    let mut w = spec.clone();
    validate_wait(&w)?;
    let id = model::string(&w, "id").unwrap_or_default();
    if model::bool_or(&s, "stopped", false) || model::dig(&s, &["waits", &id]).is_some() {
        return Err(queue_error(
            "queue stopped or wait ID already registered; use a new ID for a new attempt",
        ));
    }
    let wait_count = model::get(&s, "waits")
        .and_then(Value::as_object)
        .map(Map::len)
        .unwrap_or(0);
    if wait_count >= 1000 {
        return Err(queue_error(
            "queue reached 1000 waits; finish this batch and start a new queue",
        ));
    }
    let now = util::now();
    w["registered_at"] = json!(now);
    w["deadline"] = json!(now + model::number(&w, "timeout_seconds").unwrap_or(0.0));
    w["outcome"] = json!("pending");
    w["result"] = Value::Null;
    w["seen"] = json!([]);
    w["closed"] = json!(false);
    let waits = model::get_mut(&mut s, "waits")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| queue_error("queue waits must be a JSON object"))?;
    waits.insert(id, w.clone());
    save(root, &mut s)?;
    Ok(w)
}

pub fn signal(
    root: &Path,
    wait_id: &str,
    outcome: &str,
    summary: &str,
    evidence: Option<&str>,
) -> Result<J> {
    let s = load_queue(root)?;
    signal_checked(root, &s, wait_id, outcome, summary, evidence)
}

fn signal_checked(
    root: &Path,
    s: &J,
    wait_id: &str,
    outcome: &str,
    summary: &str,
    evidence: Option<&str>,
) -> Result<J> {
    let w = wait_for_signal(s, wait_id)?;
    signal_allowed(root, s, &w, wait_id, outcome, summary, evidence)
}

fn wait_for_signal(s: &J, wait_id: &str) -> Result<J> {
    model::dig(s, &["waits", wait_id])
        .cloned()
        .ok_or_else(|| queue_error("wait is not registered"))
}

fn signal_allowed(
    root: &Path,
    s: &J,
    w: &J,
    wait_id: &str,
    outcome: &str,
    summary: &str,
    evidence: Option<&str>,
) -> Result<J> {
    ensure_signal_allowed(s, w)?;
    signal_validated(root, s, w, wait_id, outcome, summary, evidence)
}

fn ensure_signal_allowed(s: &J, w: &J) -> Result<()> {
    match (
        model::bool_or(s, "stopped", false),
        model::bool_or(w, "closed", false),
        model::string(w, "kind").as_deref() == Some("signal"),
    ) {
        (false, false, true) => Ok(()),
        _ => Err(queue_error("wait cannot accept a producer signal")),
    }
}

fn signal_validated(
    root: &Path,
    s: &J,
    w: &J,
    wait_id: &str,
    outcome: &str,
    summary: &str,
    evidence: Option<&str>,
) -> Result<J> {
    validate_signal_input(outcome, summary)?;
    signal_producer(root, s, w, wait_id, outcome, summary, evidence)
}

fn validate_signal_input(outcome: &str, summary: &str) -> Result<()> {
    match outcome {
        "complete" | "failed" | "decision" => ensure_signal_summary(summary),
        _ => Err(queue_error("invalid outcome or summary")),
    }
}

fn ensure_signal_summary(summary: &str) -> Result<()> {
    match (summary.is_empty(), summary.chars().count() > 1000) {
        (false, false) => Ok(()),
        _ => Err(queue_error("invalid outcome or summary")),
    }
}

fn signal_producer(
    root: &Path,
    s: &J,
    w: &J,
    wait_id: &str,
    outcome: &str,
    summary: &str,
    evidence: Option<&str>,
) -> Result<J> {
    ensure_signal_producer(w)?;
    signal_store(root, s, wait_id, w, outcome, summary, evidence)
}

fn ensure_signal_producer(w: &J) -> Result<()> {
    match model::string(w, "producer_session") {
        Some(producer_session) => ensure_producer_thread(&producer_session),
        _ => Ok(()),
    }
}

fn ensure_producer_thread(producer_session: &str) -> Result<()> {
    if std::env::var("CODEX_THREAD_ID").ok().as_deref() != Some(producer_session) {
        return Err(queue_error("producer session mismatch"));
    }
    Ok(())
}

fn signal_store(
    root: &Path,
    s: &J,
    wait_id: &str,
    w: &J,
    outcome: &str,
    summary: &str,
    evidence: Option<&str>,
) -> Result<J> {
    let path = root.join("results").join(format!("{wait_id}.json"));
    let result = signal_payload(s, wait_id, w, outcome, summary, evidence);
    signal_persist(&path, &result)
}

fn signal_payload(
    s: &J,
    wait_id: &str,
    w: &J,
    outcome: &str,
    summary: &str,
    evidence: Option<&str>,
) -> J {
    json!({
        "queue": s.get("id").cloned().unwrap_or(Value::Null),
        "wait": wait_id,
        "task": w.get("task").cloned().unwrap_or(Value::Null),
        "epoch": w.get("epoch").cloned().unwrap_or(Value::Null),
        "outcome": outcome,
        "summary": summary,
        "evidence": evidence,
        "time": util::now()
    })
}

fn signal_persist(path: &Path, result: &J) -> Result<J> {
    if path.exists() {
        return signal_existing(path, result);
    }
    signal_create(path, result)
}

fn signal_create(path: &Path, result: &J) -> Result<J> {
    util::atomic_write_json(path, result)?;
    Ok(result.clone())
}

fn signal_existing(path: &Path, result: &J) -> Result<J> {
    let old = util::read_json(path)?;
    signal_existing_checked(old, result)
}

fn signal_existing_checked(old: J, result: &J) -> Result<J> {
    ensure_signal_immutable(&old, result)?;
    Ok(old)
}

fn ensure_signal_immutable(old: &J, result: &J) -> Result<()> {
    match ["outcome", "summary", "evidence"]
        .iter()
        .find(|key| model::get(old, key) != model::get(result, key))
    {
        Some(_) => Err(queue_error(
            "producer result is immutable; register a new attempt",
        )),
        _ => Ok(()),
    }
}

#[derive(Clone, Debug)]
pub struct Observed {
    pub outcome: String,
    pub summary: String,
    pub evidence: Option<String>,
    pub suffix: Option<String>,
}

fn review_overflow(w: &J, reviews: &J, evidence: &str) -> Option<Observed> {
    if model::int(reviews, "overflow_count").unwrap_or(0) <= 0 {
        return None;
    }
    review_overflow_unseen(w, reviews, evidence)
}

fn review_overflow_unseen(w: &J, reviews: &J, evidence: &str) -> Option<Observed> {
    let suffix = format!(
        "review-overflow:{}:{}",
        model::string(reviews, "nonce").unwrap_or_default(),
        model::string(reviews, "thread_id").unwrap_or_default()
    );
    let key = format!(
        "{}:decision:{suffix}",
        model::string(w, "id").unwrap_or_default()
    );
    if seen_contains(w, &key) {
        return None;
    }
    Some(Observed {
        outcome: "decision".to_string(),
        summary: "Automatic review event window overflowed; inspect native transcript and retained review-history.".to_string(),
        evidence: Some(evidence.to_string()),
        suffix: Some(suffix),
    })
}

fn review_decisions(w: &J, reviews: &J, evidence: &str) -> Option<Observed> {
    let events = model::array_field(reviews, "events")?;
    events
        .iter()
        .find_map(|event| review_event(w, event, evidence))
}

fn review_event(w: &J, event: &J, evidence: &str) -> Option<Observed> {
    if !review_event_actionable(event) {
        return None;
    }
    review_event_unseen(w, event, evidence)
}

fn review_event_actionable(event: &J) -> bool {
    let status = model::string(event, "status").unwrap_or_default();
    if !recognized_review_status(&status) {
        return false;
    }
    review_status_allowed(event, &status)
}

fn recognized_review_status(status: &str) -> bool {
    ["denied", "timedOut", "aborted", "guardianWarning"].contains(&status)
}

fn review_status_allowed(event: &J, status: &str) -> bool {
    match status {
        "guardianWarning" => model::get(event, "related_review_id").is_none(),
        _ => true,
    }
}

fn review_event_unseen(w: &J, event: &J, evidence: &str) -> Option<Observed> {
    let status = model::string(event, "status").unwrap_or_default();
    let event_id = model::string(event, "event_id").unwrap_or_default();
    let suffix = format!("review:{event_id}");
    let key = format!(
        "{}:decision:{suffix}",
        model::string(w, "id").unwrap_or_default()
    );
    if seen_contains(w, &key) {
        return None;
    }
    Some(review_observed(event, evidence, &status, suffix))
}

fn review_observed(event: &J, evidence: &str, status: &str, suffix: String) -> Observed {
    let rationale = model::string(event, "rationale")
        .unwrap_or_else(|| "Inspect native review evidence.".to_string());
    let summary = format!("Automatic review {status}: {rationale}");
    Observed {
        outcome: "decision".to_string(),
        summary: rationale_prefix(summary),
        evidence: Some(evidence.to_string()),
        suffix: Some(suffix),
    }
}

fn seen_contains(w: &J, key: &str) -> bool {
    model::array_field(w, "seen")
        .map(|seen| seen.iter().any(|value| value.as_str() == Some(key)))
        .unwrap_or(false)
}

fn rationale_prefix(value: String) -> String {
    value.chars().take(1000).collect()
}

fn observe_guard(_root: &Path, w: &J, _now: f64) -> Result<Option<Observed>> {
    let source = model::string(w, "source").unwrap_or_default();
    let g = observe_guard_state(&source)?;
    observe_guard_checked(&source, w, &g)
}

fn observe_guard_state(source: &str) -> Result<J> {
    util::read_state_json(Path::new(source), "state.json")
}

fn observe_guard_checked(source: &str, w: &J, g: &J) -> Result<Option<Observed>> {
    ensure_guard_scope(w, g)?;
    observe_guard_phase(source, w, g)
}

fn ensure_guard_scope(w: &J, g: &J) -> Result<()> {
    if guard::scope(g)
        != model::get(w, "guard_identity")
            .cloned()
            .unwrap_or(Value::Null)
    {
        return Err(queue_error(
            "worker guard registration changed; inspect before rebinding",
        ));
    }
    Ok(())
}

fn observe_guard_phase(source: &str, w: &J, g: &J) -> Result<Option<Observed>> {
    let phase = model::string(g, "phase").unwrap_or_default();
    if phase == "needs_attention" {
        return Ok(Some(guard_attention_event(source, g)));
    }
    observe_guard_phase_allowed(source, w, g, &phase)
}

fn observe_guard_phase_allowed(
    source: &str,
    w: &J,
    g: &J,
    phase: &str,
) -> Result<Option<Observed>> {
    ensure_guard_phase_allowed(phase)?;
    observe_guard_watcher(source, w, g)
}

fn guard_attention_event(source: &str, g: &J) -> Observed {
    Observed {
        outcome: "decision".to_string(),
        summary: format!(
            "Worker guard needs attention: {}",
            model::string(g, "error").unwrap_or_else(|| "unknown error".to_string())
        ),
        evidence: Some(Path::new(source).join("state.json").display().to_string()),
        suffix: Some("guard-attention".to_string()),
    }
}

fn guard_allowed_phases() -> BTreeSet<String> {
    [
        "monitoring",
        "parked",
        "requested",
        "request_sending",
        "ready",
    ]
    .iter()
    .map(|value| value.to_string())
    .chain(
        guard::ROTATION_PHASES
            .iter()
            .map(|value| (*value).to_string()),
    )
    .collect()
}

fn ensure_guard_phase_allowed(phase: &str) -> Result<()> {
    if !guard_allowed_phases().contains(phase) {
        return Err(queue_error(
            "worker guard is released or has an unknown phase; inspect lifecycle",
        ));
    }
    Ok(())
}

fn observe_guard_watcher(source: &str, w: &J, g: &J) -> Result<Option<Observed>> {
    let watcher = watch_service::health(Path::new(source));
    if !model::bool_or(&watcher, "healthy", false) {
        return Ok(Some(guard_watcher_event(source, &watcher)));
    }
    observe_guard_reviews(source, w, g)
}

fn guard_watcher_event(source: &str, watcher: &J) -> Observed {
    Observed {
        outcome: "decision".to_string(),
        summary: format!(
            "Worker watcher is not healthy: {}",
            model::string(watcher, "reason").unwrap_or_default()
        ),
        evidence: Some(Path::new(source).join("watch.json").display().to_string()),
        suffix: Some("guard-watcher".to_string()),
    }
}

fn observe_guard_reviews(source: &str, w: &J, g: &J) -> Result<Option<Observed>> {
    let reviews = guard::review_events(Path::new(source), g)?;
    let evidence = Path::new(source)
        .join("review-events.json")
        .display()
        .to_string();
    observe_guard_review_policy(w, g, &reviews, &evidence)
}

fn observe_guard_review_policy(
    w: &J,
    g: &J,
    reviews: &J,
    evidence: &str,
) -> Result<Option<Observed>> {
    if review_policy_stale(g, reviews) {
        return Ok(Some(Observed {
            outcome: "decision".to_string(),
            summary:
                "Automatic review observation is unavailable or stale; inspect the native bridge."
                    .to_string(),
            evidence: Some(evidence.to_string()),
            suffix: Some("review-observation".to_string()),
        }));
    }
    Ok(review_overflow(w, reviews, evidence).or_else(|| review_decisions(w, reviews, evidence)))
}

fn review_policy_stale(g: &J, reviews: &J) -> bool {
    review_policy_enabled(g) && review_observations_stale(g, reviews)
}

fn review_policy_enabled(g: &J) -> bool {
    model::get(g, "review_policy")
        .map(|value| model::truthy(Some(value)))
        .unwrap_or(false)
}

fn review_observations_stale(g: &J, reviews: &J) -> bool {
    model::string(reviews, "status").as_deref() != Some("observed") && review_phase_guarded(g)
}

fn review_phase_guarded(g: &J) -> bool {
    let phase = model::string(g, "phase").unwrap_or_default();
    ["monitoring", "parked", "requested", "ready"].contains(&phase.as_str())
}

fn observe_signal(root: &Path, s: &J, w: &J) -> Result<Option<Observed>> {
    let wait_id = model::string(w, "id").unwrap_or_default();
    let path = root.join("results").join(format!("{wait_id}.json"));
    if !path.exists() {
        return Ok(None);
    }
    let r = util::read_json(&path)?;
    let expected = [
        ("queue", s.get("id").cloned().unwrap_or(Value::Null)),
        ("wait", json!(wait_id)),
        ("task", w.get("task").cloned().unwrap_or(Value::Null)),
        ("epoch", w.get("epoch").cloned().unwrap_or(Value::Null)),
    ];
    for (key, expected) in expected {
        if model::get(&r, key).cloned().unwrap_or(Value::Null) != expected {
            return Err(queue_error("producer signal identity mismatch"));
        }
    }
    let outcome = model::string(&r, "outcome").unwrap_or_default();
    if !matches!(outcome.as_str(), "complete" | "failed" | "decision") {
        return Err(queue_error("unknown signal outcome"));
    }
    Ok(Some(Observed {
        outcome,
        summary: model::string(&r, "summary").unwrap_or_default(),
        evidence: Some(path.display().to_string()),
        suffix: None,
    }))
}

const CI_OUTCOMES: &[(&str, &str, Option<&str>)] = &[
    ("success", "complete", None),
    ("failure", "failed", None),
    ("timeout", "timeout", None),
    ("superseded", "decision", Some("ci-superseded")),
    ("needs_attention", "decision", Some("ci-needs_attention")),
];

fn observe_ci(root: &Path, w: &mut J, now: f64) -> Result<Option<Observed>> {
    let _ = root;
    let source = PathBuf::from(model::string(w, "source").unwrap_or_default());
    if !ci_source_available(&source) {
        return Ok(None);
    }
    observe_ci_identity(&source, w, now)
}

fn ci_source_available(source: &Path) -> bool {
    source.join("identity.json").exists() && source.join("status.json").exists()
}

fn observe_ci_identity(source: &Path, w: &mut J, now: f64) -> Result<Option<Observed>> {
    let identity = util::read_state_json(source, "identity.json")?;
    observe_ci_status(source, w, now, &identity)
}

fn observe_ci_status(source: &Path, w: &mut J, now: f64, identity: &J) -> Result<Option<Observed>> {
    let status = util::read_state_json(source, "status.json")?;
    observe_ci_pinned(source, w, now, identity, &status)
}

fn observe_ci_pinned(
    source: &Path,
    w: &mut J,
    now: f64,
    identity: &J,
    status: &J,
) -> Result<Option<Observed>> {
    let pinned = model::string(w, "config_hash").unwrap_or_default();
    ensure_ci_pinned(identity, status, &pinned)?;
    observe_ci_config(source, w, now, identity, &pinned, status)
}

fn ensure_ci_pinned(identity: &J, status: &J, pinned: &str) -> Result<()> {
    match (
        model::string(identity, "config_hash").as_deref() == Some(pinned),
        model::string(status, "config_hash").as_deref() == Some(pinned),
    ) {
        (true, true) => Ok(()),
        _ => Err(queue_error("CI configuration identity mismatch")),
    }
}

fn observe_ci_config(
    source: &Path,
    w: &mut J,
    now: f64,
    identity: &J,
    pinned: &str,
    status: &J,
) -> Result<Option<Observed>> {
    let config = ci_config(identity)?;
    observe_ci_config_checked(source, w, now, &config, pinned, status)
}

fn ci_config(identity: &J) -> Result<J> {
    model::get(identity, "config")
        .cloned()
        .ok_or_else(|| queue_error("CI task/epoch/config mismatch"))
}

fn observe_ci_config_checked(
    source: &Path,
    w: &mut J,
    now: f64,
    config: &J,
    pinned: &str,
    status: &J,
) -> Result<Option<Observed>> {
    ensure_ci_config(w, config, pinned)?;
    observe_ci_state(source, w, now, status)
}

fn ensure_ci_config(w: &J, config: &J, pinned: &str) -> Result<()> {
    match (
        sha256_json_sorted(config) != pinned,
        model::get(config, "task") != model::get(w, "task"),
        model::get(config, "epoch") != model::get(w, "epoch"),
    ) {
        (false, false, false) => Ok(()),
        _ => Err(queue_error("CI task/epoch/config mismatch")),
    }
}

fn observe_ci_state(source: &Path, w: &mut J, now: f64, status: &J) -> Result<Option<Observed>> {
    let state = model::string(status, "state").unwrap_or_default();
    if state == "api_error" {
        return observe_ci_api_error(source, w, now);
    }
    observe_ci_active_state(source, w, &state)
}

fn observe_ci_api_error(source: &Path, w: &mut J, now: f64) -> Result<Option<Observed>> {
    if model::get(w, "api_error_since").is_none() {
        w["api_error_since"] = json!(now);
    }
    let since = model::number(w, "api_error_since").unwrap_or(now);
    observe_ci_api_wait(source, now, since)
}

fn observe_ci_api_wait(source: &Path, now: f64, since: f64) -> Result<Option<Observed>> {
    if now - since >= 120.0 {
        return Ok(Some(Observed {
            outcome: "decision".to_string(),
            summary: "CI observer has reported API errors for at least 120 seconds".to_string(),
            evidence: Some(source.join("status.json").display().to_string()),
            suffix: Some("ci-api-error".to_string()),
        }));
    }
    Ok(None)
}

fn observe_ci_active_state(source: &Path, w: &mut J, state: &str) -> Result<Option<Observed>> {
    model::remove(w, "api_error_since");
    if ["pending", "running"].contains(&state) {
        return Ok(None);
    }
    observe_ci_final(source, state)
}

fn observe_ci_final(source: &Path, state: &str) -> Result<Option<Observed>> {
    let (outcome, suffix) = ci_outcome(state)?;
    Ok(Some(Observed {
        outcome: outcome.to_string(),
        summary: format!("CI {state}; inspect the bound run/job evidence"),
        evidence: Some(source.join("status.json").display().to_string()),
        suffix: suffix.map(ToString::to_string),
    }))
}

fn ci_outcome(state: &str) -> Result<(&'static str, Option<&'static str>)> {
    CI_OUTCOMES
        .iter()
        .find(|(name, _, _)| *name == state)
        .map(|(_, outcome, suffix)| (*outcome, *suffix))
        .ok_or_else(|| queue_error("unrecognized CI observer state"))
}

pub fn observation(root: &Path, s: &J, w: &mut J, now: f64) -> Result<Option<Observed>> {
    match model::string(w, "kind").as_deref() {
        Some("guard") => observe_guard(root, w, now),
        Some("signal") => observe_signal(root, s, w),
        _ => observe_ci(root, w, now),
    }
}

pub fn collect(root: &Path, s: &mut J, now: f64) -> Result<()> {
    let queue_id = model::string(s, "id").unwrap_or_default();
    let wait_ids = collect_wait_ids(s);
    collect_each(root, s, now, &queue_id, &wait_ids)
}

fn collect_wait_ids(s: &J) -> Vec<String> {
    model::get(s, "waits")
        .and_then(Value::as_object)
        .map(|waits| waits.keys().cloned().collect())
        .unwrap_or_default()
}

fn collect_each(
    root: &Path,
    s: &mut J,
    now: f64,
    queue_id: &str,
    wait_ids: &[String],
) -> Result<()> {
    wait_ids
        .iter()
        .try_for_each(|wait_id| collect_one(root, s, now, queue_id, wait_id))
}

fn collect_one(root: &Path, s: &mut J, now: f64, queue_id: &str, wait_id: &str) -> Result<()> {
    let Some(original) = model::dig(s, &["waits", wait_id]).cloned() else {
        return Ok(());
    };
    if model::bool_or(&original, "closed", false) {
        return Ok(());
    }
    collect_open(root, s, now, queue_id, wait_id, original)
}

fn collect_open(
    root: &Path,
    s: &mut J,
    now: f64,
    queue_id: &str,
    wait_id: &str,
    original: J,
) -> Result<()> {
    let mut w = original;
    let observed = wait_observation(root, s, &mut w, now);
    collect_observed(s, now, queue_id, wait_id, w, observed)
}

fn wait_observation(root: &Path, s: &J, w: &mut J, now: f64) -> Option<Observed> {
    let observed =
        observation(root, s, w, now).unwrap_or_else(|error| Some(source_error_observed(w, error)));
    observed.or_else(|| deadline_observed(w, now))
}

fn source_error_observed(w: &J, error: FlowError) -> Observed {
    let summary = if error.kind == ErrorKind::Queue {
        error.message.clone()
    } else {
        "source unavailable or malformed".to_string()
    };
    let suffix = format!("source-error:{}", &sha256_hex(summary.as_bytes())[..16]);
    Observed {
        outcome: "decision".to_string(),
        summary,
        evidence: model::string(w, "source"),
        suffix: Some(suffix),
    }
}

fn deadline_observed(w: &J, now: f64) -> Option<Observed> {
    if now >= model::number(w, "deadline").unwrap_or(f64::INFINITY) {
        return Some(Observed {
            outcome: "timeout".to_string(),
            summary: format!(
                "Waiting deadline exceeded: {}",
                model::string(w, "condition").unwrap_or_default()
            ),
            evidence: model::string(w, "source"),
            suffix: None,
        });
    }
    None
}

fn collect_observed(
    s: &mut J,
    now: f64,
    queue_id: &str,
    wait_id: &str,
    w: J,
    observed: Option<Observed>,
) -> Result<()> {
    let Some(observed) = observed else {
        s["waits"][wait_id] = w;
        return Ok(());
    };
    apply_observation(s, now, queue_id, wait_id, w, observed)
}

fn apply_observation(
    s: &mut J,
    now: f64,
    queue_id: &str,
    wait_id: &str,
    mut w: J,
    observed: Observed,
) -> Result<()> {
    w["outcome"] = json!(observed.outcome.clone());
    w["result"] = json!(observed.summary.clone());
    let key = observation_key(wait_id, &observed);
    if seen_contains(&w, &key) {
        s["waits"][wait_id] = w;
        return Ok(());
    }
    commit_observation(s, now, queue_id, wait_id, w, observed, key)
}

fn observation_key(wait_id: &str, observed: &Observed) -> String {
    let mut key = format!("{wait_id}:{}", observed.outcome);
    if let Some(suffix) = &observed.suffix {
        key.push(':');
        key.push_str(suffix);
    }
    key
}

fn seen_with(w: &J, key: &str) -> Vec<J> {
    let mut seen = model::get(w, "seen")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    seen.push(json!(key));
    seen
}

fn event_capacity_available(s: &J) -> bool {
    model::get(s, "events")
        .and_then(Value::as_object)
        .map(Map::len)
        .unwrap_or(0)
        < 10000
}

fn commit_observation(
    s: &mut J,
    now: f64,
    queue_id: &str,
    wait_id: &str,
    mut w: J,
    observed: Observed,
    key: String,
) -> Result<()> {
    w["seen"] = json!(seen_with(&w, &key));
    let event_id = sha256_hex(format!("{queue_id}:{key}").as_bytes())[..24].to_string();
    let room = event_capacity_available(s);
    save_observation(s, now, wait_id, w, observed, event_id, room)
}

fn save_observation(
    s: &mut J,
    now: f64,
    wait_id: &str,
    w: J,
    observed: Observed,
    event_id: String,
    room: bool,
) -> Result<()> {
    if !room {
        return Err(queue_error(
            "queue reached 10000 events; retain this queue and start a new one",
        ));
    }
    attach_observation(s, now, wait_id, w, observed, event_id)
}

fn attach_observation(
    s: &mut J,
    now: f64,
    wait_id: &str,
    w: J,
    observed: Observed,
    event_id: String,
) -> Result<()> {
    let mut event = observation_event(&event_id, wait_id, now, &observed, &w);
    progress::attach(s, &mut event, &w, now)?;
    s["events"][&event_id] = event;
    s["waits"][wait_id] = w;
    Ok(())
}

fn observation_event(event_id: &str, wait_id: &str, now: f64, observed: &Observed, w: &J) -> J {
    json!({
        "id": event_id,
        "wait": wait_id,
        "task": w.get("task").cloned().unwrap_or(Value::Null),
        "epoch": w.get("epoch").cloned().unwrap_or(Value::Null),
        "owner": w.get("owner").cloned().unwrap_or(Value::Null),
        "outcome": observed.outcome,
        "summary": observed.summary,
        "evidence": observed.evidence,
        "observed_at": now,
        "acknowledged": false
    })
}

fn guard_root(s: &J) -> Result<PathBuf> {
    match model::string(s, "guard_dir") {
        Some(path) if !path.is_empty() => Ok(PathBuf::from(path)),
        _ => Err(queue_error("queue has no receiver guard")),
    }
}

pub fn operator_notice(
    root: &Path,
    s: &mut J,
    delivery: &mut J,
    field: &str,
    title: &str,
    body: &str,
    factory: &ReceiverFactory,
) -> Result<()> {
    if model::get(delivery, field).is_some() {
        return Ok(());
    }
    operator_notice_attempt(root, s, delivery, field, title, body, factory)
}

fn operator_notice_attempt(
    root: &Path,
    s: &mut J,
    delivery: &mut J,
    field: &str,
    title: &str,
    body: &str,
    factory: &ReceiverFactory,
) -> Result<()> {
    delivery[field] = json!("attempted");
    store_delivery_attempt(s, delivery);
    operator_notice_save(root, s, delivery, field, title, body, factory)
}

fn store_delivery_attempt(s: &mut J, delivery: &J) {
    if let Some(delivery_id) = model::string(delivery, "id") {
        s["deliveries"][&delivery_id] = delivery.clone();
    }
}

fn operator_notice_save(
    root: &Path,
    s: &mut J,
    delivery: &mut J,
    field: &str,
    title: &str,
    body: &str,
    factory: &ReceiverFactory,
) -> Result<()> {
    save(root, s)?;
    operator_notice_guard(root, s, delivery, field, title, body, factory)
}

fn operator_notice_guard(
    root: &Path,
    s: &mut J,
    delivery: &mut J,
    field: &str,
    title: &str,
    body: &str,
    factory: &ReceiverFactory,
) -> Result<()> {
    let guard_dir = guard_root(s)?;
    operator_notice_submit(
        root,
        s,
        delivery,
        &NoticeText { field, title, body },
        factory,
        &guard_dir,
    )
}

fn operator_notice_submit(
    root: &Path,
    s: &mut J,
    delivery: &mut J,
    notice_text: &NoticeText<'_>,
    factory: &ReceiverFactory,
    guard_dir: &Path,
) -> Result<()> {
    let result = operator_notice_call(
        root,
        s,
        delivery,
        notice_text.title,
        notice_text.body,
        factory,
        guard_dir,
    );
    apply_notice_result(delivery, notice_text.field, result.err());
    Ok(())
}

fn apply_notice_result(delivery: &mut J, field: &str, error: Option<FlowError>) {
    if let Some(error) = error {
        delivery[field] = json!("failed");
        delivery[format!("{field}_error")] = json!(error.kind.type_name());
    } else {
        delivery[field] = json!("submitted");
    }
}

fn operator_notice_call(
    root: &Path,
    s: &J,
    delivery: &J,
    title: &str,
    body: &str,
    factory: &ReceiverFactory,
    guard_dir: &Path,
) -> Result<()> {
    let g = util::read_state_json(guard_dir, "state.json")?;
    operator_notice_receiver(root, s, delivery, title, body, factory, &g)
}

fn operator_notice_receiver(
    root: &Path,
    s: &J,
    delivery: &J,
    title: &str,
    body: &str,
    factory: &ReceiverFactory,
    g: &J,
) -> Result<()> {
    ensure_notice_receiver(s, g, delivery)?;
    operator_notice_send(root, delivery, title, body, factory, g)
}

fn ensure_notice_receiver(s: &J, g: &J, delivery: &J) -> Result<()> {
    match (
        guard::scope(g) == model::get(s, "receiver").cloned().unwrap_or(Value::Null),
        model::string(g, "session") == model::string(delivery, "session"),
    ) {
        (true, true) => Ok(()),
        _ => Err(queue_error("receiver changed before operator notification")),
    }
}

fn operator_notice_send(
    root: &Path,
    delivery: &J,
    title: &str,
    body: &str,
    factory: &ReceiverFactory,
    g: &J,
) -> Result<()> {
    let adapter = factory(g)?;
    let delivery_id = model::string(delivery, "id").unwrap_or_default();
    adapter
        .call(&[
            "notification".to_string(),
            "show".to_string(),
            title.to_string(),
            "--body".to_string(),
            format!(
                "{}: delivery {} {} Inspect the delivery and receiver before ACK or resolve; do not resend.",
                root.display(),
                delivery_id,
                body
            ),
            "--sound".to_string(),
            "request".to_string(),
        ])
        .map(|_| ())
}

fn active_delivery_id(s: &J) -> Result<String> {
    model::string(s, "active").ok_or_else(|| queue_error("no active delivery"))
}

fn load_delivery_for(s: &J, active: &str) -> Result<(String, J)> {
    let delivery = model::dig(s, &["deliveries", active])
        .cloned()
        .ok_or_else(|| queue_error("no active delivery"))?;
    Ok((active.to_string(), delivery))
}

fn load_active_delivery(s: &J) -> Result<(String, J)> {
    let active = active_delivery_id(s)?;
    load_delivery_for(s, &active)
}

const DELIVERY_NOTICES: &[(&str, &str, &str, &str)] = &[
    (
        "ack_overdue",
        "overdue_notice",
        "Herdr flow ACK overdue",
        "has no ACK after ten minutes.",
    ),
    (
        "uncertain",
        "uncertain_notice",
        "Herdr flow delivery uncertain",
        "may or may not have reached the receiver.",
    ),
];

fn notice_for_phase(phase: &str) -> Option<(&'static str, &'static str, &'static str)> {
    DELIVERY_NOTICES
        .iter()
        .find(|(name, _, _, _)| *name == phase)
        .map(|(_, field, title, body)| (*field, *title, *body))
}

fn mark_delivery_uncertain(delivery: &mut J) {
    if model::string(delivery, "phase").as_deref() == Some("sending") {
        delivery["phase"] = json!("uncertain");
    }
}

fn delivery_submitted(delivery: &J) -> bool {
    model::string(delivery, "phase").as_deref() == Some("submitted")
}

fn delivery_age(delivery: &J) -> f64 {
    util::now() - model::number(delivery, "sent_at").unwrap_or(0.0)
}

fn delivery_is_overdue(delivery: &J) -> bool {
    delivery_submitted(delivery) && delivery_age(delivery) >= 600.0
}

fn mark_delivery_overdue(delivery: &mut J) {
    if delivery_is_overdue(delivery) {
        delivery["phase"] = json!("ack_overdue");
    }
}

fn refresh_delivery_phase(delivery: &mut J) {
    mark_delivery_uncertain(delivery);
    mark_delivery_overdue(delivery);
}

fn persist_active_delivery(root: &Path, s: &mut J, active: &str, delivery: &J) -> Result<()> {
    s["deliveries"][active] = delivery.clone();
    save(root, s)
}

fn apply_delivery_notice(
    root: &Path,
    s: &mut J,
    delivery: &mut J,
    factory: &ReceiverFactory,
    active: &str,
    notice: Option<(&'static str, &'static str, &'static str)>,
) -> Result<()> {
    let Some((field, title, body)) = notice else {
        return persist_active_delivery(root, s, active, delivery);
    };
    operator_notice(root, s, delivery, field, title, body, factory)?;
    persist_active_delivery(root, s, active, delivery)
}

fn update_delivery_phase(
    root: &Path,
    s: &mut J,
    factory: &ReceiverFactory,
    active: &str,
    delivery: &mut J,
) -> Result<()> {
    refresh_delivery_phase(delivery);
    let phase = model::string(delivery, "phase").unwrap_or_default();
    s["delivery_status"] = json!(phase);
    let notice = notice_for_phase(&phase);
    apply_delivery_notice(root, s, delivery, factory, active, notice)
}

fn update_active_delivery(root: &Path, s: &mut J, factory: &ReceiverFactory) -> Result<()> {
    let (active, mut delivery) = load_active_delivery(s)?;
    update_delivery_phase(root, s, factory, &active, &mut delivery)
}

fn delivery_message(root: &Path, delivery: &str) -> String {
    let executable = util::current_exe()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| "herdr-flow".to_string());
    format!(
        "Wait queue has actionable event(s). Read only {}/deliveries/{delivery}.json and relevant task handoffs. Before acting, acknowledge receipt with: {} queue ack --state-dir {} --delivery {delivery}. On a lock-busy error retry after one second; on any other ACK failure stop. ACK confirms receipt only, not success or merge permission. Process the recorded outcomes within existing authorization. For managed tasks, read tasks in queue.json and use task-apply to persist result application and the concrete next action; ACK leaves results unapplied. Recheck progress_stall events with event-current before recovery; stale alarms grant no action. Register any new waits and return idle when all remaining work is waiting; do not poll agents or CI in a model loop.",
        root.display(),
        util::shell_quote(&executable),
        util::shell_quote(&root.display().to_string())
    )
}

fn send_delivery(
    root: &Path,
    s: &mut J,
    g: &mut J,
    guard_dir: &Path,
    pending: &[J],
    adapter: &dyn crate::herdr::ReceiverAdapter,
) -> Result<()> {
    let batch: Vec<J> = pending.iter().take(20).cloned().collect();
    let delivery_count = model::get(s, "deliveries")
        .and_then(Value::as_object)
        .map(Map::len)
        .unwrap_or(0);
    if delivery_count >= 10000 {
        return Err(queue_error(
            "queue reached 10000 deliveries; preserve this queue before starting a new one",
        ));
    }
    let delivery_id = uuid_hex();
    let event_ids: Vec<J> = batch
        .iter()
        .map(|event| event.get("id").cloned().unwrap_or(Value::Null))
        .collect();
    let mut d = json!({
        "id": delivery_id,
        "events": event_ids,
        "session": g.get("session").cloned().unwrap_or(Value::Null),
        "phase": "sending",
        "sent_at": util::now()
    });
    s["deliveries"][&delivery_id] = d.clone();
    s["active"] = json!(delivery_id);
    logging::log_event_fields(
        "delivery_created",
        "wait_queue",
        &format!(
            "Created delivery {delivery_id} with {} event(s)",
            batch.len()
        ),
        logging::Fields {
            metadata: Some(&json!({
                "delivery_id": delivery_id,
                "event_count": batch.len(),
                "receiver": s.get("receiver")
            })),
            ..Default::default()
        },
    );
    // Persist the delivery before any app-server submission. A crash never replays it.
    save(root, s)?;
    g["external_delivery"] = json!({
        "queue": root.display().to_string(),
        "delivery": delivery_id,
        "session": g.get("session").cloned().unwrap_or(Value::Null)
    });
    guard::save(guard_dir, g)?;
    util::atomic_write_json(
        &root.join("deliveries").join(format!("{delivery_id}.json")),
        &json!({"delivery": d.clone(), "events": batch}),
    )?;
    let message = delivery_message(root, &delivery_id);
    match adapter.prompt(&message) {
        Ok(turn_id) => {
            d["turn_id"] = json!(turn_id);
            d["phase"] = json!("submitted");
        }
        Err(error)
            if matches!(
                error.kind,
                ErrorKind::Guard
                    | ErrorKind::AgentBusy
                    | ErrorKind::Transient
                    | ErrorKind::Io
                    | ErrorKind::Json
                    | ErrorKind::Timeout
            ) =>
        {
            d["phase"] = json!("uncertain");
        }
        Err(error) => return Err(error),
    }
    s["deliveries"][&delivery_id] = d.clone();
    let phase = model::string(&d, "phase").unwrap_or_default();
    s["delivery_status"] = json!(phase);
    save(root, s)?;
    Ok(())
}

fn deliver_locked(
    root: &Path,
    s: &mut J,
    pending: &[J],
    guard_dir: &Path,
    factory: &ReceiverFactory,
) -> Result<()> {
    let mut g = util::read_state_json(guard_dir, "state.json")?;
    deliver_guard_checked(root, s, pending, guard_dir, factory, &mut g)
}

fn deliver_guard_checked(
    root: &Path,
    s: &mut J,
    pending: &[J],
    guard_dir: &Path,
    factory: &ReceiverFactory,
    g: &mut J,
) -> Result<()> {
    ensure_receiver_scope(s, g)?;
    deliver_phase_checked(root, s, pending, guard_dir, factory, g)
}

fn deliver_phase_checked(
    root: &Path,
    s: &mut J,
    pending: &[J],
    guard_dir: &Path,
    factory: &ReceiverFactory,
    g: &mut J,
) -> Result<()> {
    ensure_guard_attention(g)?;
    deliver_ready_state(root, s, pending, guard_dir, factory, g)
}

fn ensure_guard_attention(g: &J) -> Result<()> {
    if model::string(g, "phase").as_deref() == Some("needs_attention") {
        return Err(queue_error(format!(
            "receiver guard needs attention: {}",
            model::string(g, "error").unwrap_or_else(|| "unknown error".to_string())
        )));
    }
    Ok(())
}

fn receiver_monitoring(g: &J) -> bool {
    model::string(g, "phase").as_deref() == Some("monitoring")
}

fn receiver_available(g: &J) -> bool {
    !model::bool_or(g, "rotation_due", false) && !has_external_delivery(g)
}

fn receiver_can_deliver(g: &J) -> bool {
    receiver_monitoring(g) && receiver_available(g)
}

fn deliver_ready_state(
    root: &Path,
    s: &mut J,
    pending: &[J],
    guard_dir: &Path,
    factory: &ReceiverFactory,
    g: &mut J,
) -> Result<()> {
    if !receiver_can_deliver(g) {
        return deliver_handoff(root, s);
    }
    deliver_watcher(root, s, pending, guard_dir, factory, g)
}

fn deliver_handoff(root: &Path, s: &mut J) -> Result<()> {
    s["delivery_status"] = json!("receiver_handoff_or_delivery");
    save(root, s)
}

fn deliver_watcher(
    root: &Path,
    s: &mut J,
    pending: &[J],
    guard_dir: &Path,
    factory: &ReceiverFactory,
    g: &mut J,
) -> Result<()> {
    ensure_watcher_healthy(guard_dir)?;
    deliver_adapter(root, s, pending, guard_dir, factory, g)
}

fn deliver_adapter(
    root: &Path,
    s: &mut J,
    pending: &[J],
    guard_dir: &Path,
    factory: &ReceiverFactory,
    g: &mut J,
) -> Result<()> {
    let adapter = factory(g)?;
    deliver_agent(root, s, pending, guard_dir, g, adapter.as_ref())
}

fn deliver_agent(
    root: &Path,
    s: &mut J,
    pending: &[J],
    guard_dir: &Path,
    g: &mut J,
    adapter: &dyn crate::herdr::ReceiverAdapter,
) -> Result<()> {
    let agent = adapter.agent()?;
    deliver_agent_checked(root, s, pending, guard_dir, g, adapter, &agent)
}

fn deliver_agent_checked(
    root: &Path,
    s: &mut J,
    pending: &[J],
    guard_dir: &Path,
    g: &mut J,
    adapter: &dyn crate::herdr::ReceiverAdapter,
    agent: &J,
) -> Result<()> {
    ensure_agent_session(g, agent)?;
    deliver_if_idle(root, s, pending, guard_dir, g, adapter, agent)
}

fn agent_ready(agent: &J) -> bool {
    ["idle", "done"].contains(
        &model::string(agent, "agent_status")
            .unwrap_or_default()
            .as_str(),
    ) && model::bool_or(agent, "interactive_ready", true)
}

fn deliver_if_idle(
    root: &Path,
    s: &mut J,
    pending: &[J],
    guard_dir: &Path,
    g: &mut J,
    adapter: &dyn crate::herdr::ReceiverAdapter,
    agent: &J,
) -> Result<()> {
    if !agent_ready(agent) {
        return deliver_busy(root, s);
    }
    send_delivery(root, s, g, guard_dir, pending, adapter)
}

fn deliver_busy(root: &Path, s: &mut J) -> Result<()> {
    s["delivery_status"] = json!("receiver_busy_or_blocked");
    save(root, s)
}

fn delivery_error_message(error: &FlowError) -> String {
    match error.kind {
        ErrorKind::Queue | ErrorKind::Guard => error.message.clone(),
        _ => error.kind.type_name().to_string(),
    }
}

fn deliver_locked_failure(root: &Path, s: &mut J, error: &FlowError) -> Result<()> {
    s["delivery_status"] = json!("needs_attention");
    s["delivery_error"] = json!(delivery_error_message(error));
    save(root, s)
}

fn deliver_locked_aggregate(
    root: &Path,
    s: &mut J,
    pending: &[J],
    guard_dir: &Path,
    factory: &ReceiverFactory,
) -> Result<()> {
    if let Err(error) = deliver_locked(root, s, pending, guard_dir, factory) {
        return deliver_locked_failure(root, s, &error);
    }
    Ok(())
}

fn deliver_lock_failed(root: &Path, s: &mut J, error: &FlowError) -> Result<()> {
    if error.kind == ErrorKind::StateBusy {
        s["delivery_status"] = json!("receiver_locked");
        return save(root, s);
    }
    s["delivery_status"] = json!("needs_attention");
    s["delivery_error"] = json!(error.message.clone());
    save(root, s)
}

fn deliver_with_guard(
    root: &Path,
    s: &mut J,
    pending: &[J],
    factory: &ReceiverFactory,
    guard_dir: &Path,
) -> Result<()> {
    let lock = guard::locked(guard_dir);
    if let Err(error) = &lock {
        return deliver_lock_failed(root, s, error);
    }
    deliver_locked_aggregate(root, s, pending, guard_dir, factory)
}

fn deliver_pending(root: &Path, s: &mut J, pending: &[J], factory: &ReceiverFactory) -> Result<()> {
    let guard_dir = guard_root(s)?;
    deliver_with_guard(root, s, pending, factory, &guard_dir)
}

pub fn dispatch(root: &Path, s: &mut J, factory: &ReceiverFactory) -> Result<()> {
    let pending: Vec<J> = model::get(s, "events")
        .and_then(Value::as_object)
        .map(|events| {
            events
                .values()
                .filter(|event| !model::bool_or(event, "acknowledged", false))
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    if model::get(s, "active")
        .map(|value| model::truthy(Some(value)))
        .unwrap_or(false)
    {
        return update_active_delivery(root, s, factory);
    }
    if pending.is_empty()
        || !model::bool_or(s, "notify", false)
        || !model::get(s, "guard_dir")
            .map(|value| model::truthy(Some(value)))
            .unwrap_or(false)
    {
        s["delivery_status"] = json!(if !model::bool_or(s, "notify", false) {
            "disabled"
        } else {
            "waiting_for_events"
        });
        save(root, s)?;
        return Ok(());
    }
    deliver_pending(root, s, &pending, factory)
}

pub fn tick(root: &Path, factory: &ReceiverFactory, now: Option<f64>) -> Result<J> {
    let mut s = load_queue(root)?;
    if model::bool_or(&s, "stopped", false) {
        return Ok(s);
    }
    let now = now.unwrap_or_else(util::now);
    collect(root, &mut s, now)?;
    progress::stalls(&mut s, now)?;
    s["heartbeat_at"] = json!(util::now());
    save(root, &mut s)?;
    dispatch(root, &mut s, factory)?;
    Ok(s)
}

pub fn acknowledge(root: &Path, delivery: &str, session: &str) -> Result<J> {
    let s = util::read_json(&root.join("queue.json"))?;
    acknowledge_state(root, s, delivery, session)
}

fn delivery_record(s: &J, delivery: &str) -> Result<J> {
    model::dig(s, &["deliveries", delivery])
        .cloned()
        .ok_or_else(|| queue_error("delivery is not registered"))
}

fn ensure_active_matches(s: &J, delivery: &str) -> Result<()> {
    if model::string(s, "active").as_deref() != Some(delivery) {
        return Err(queue_error("delivery is no longer active"));
    }
    Ok(())
}

fn acknowledge_state(root: &Path, s: J, delivery: &str, session: &str) -> Result<J> {
    let d = delivery_record(&s, delivery)?;
    acknowledge_session(root, s, delivery, session, d)
}

fn acknowledge_session(root: &Path, s: J, delivery: &str, session: &str, d: J) -> Result<J> {
    ensure_delivery_session(&d, session)?;
    acknowledge_phase(root, s, delivery, session, d)
}

fn ensure_delivery_session(d: &J, session: &str) -> Result<()> {
    if model::string(d, "session").as_deref() != Some(session) {
        return Err(queue_error("only the notified coordinator session can ACK"));
    }
    Ok(())
}

fn acknowledge_phase(root: &Path, s: J, delivery: &str, session: &str, d: J) -> Result<J> {
    if model::string(&d, "phase").as_deref() == Some("acknowledged") {
        return Ok(s);
    }
    acknowledge_active(root, s, delivery, session, d)
}

fn acknowledge_active(root: &Path, s: J, delivery: &str, session: &str, d: J) -> Result<J> {
    ensure_active_matches(&s, delivery)?;
    acknowledge_guard(root, s, delivery, session, d)
}

fn acknowledge_guard(root: &Path, s: J, delivery: &str, session: &str, d: J) -> Result<J> {
    let guard_dir = guard_root(&s)?;
    acknowledge_locked(root, s, delivery, session, d, &guard_dir)
}

fn acknowledge_locked(
    root: &Path,
    s: J,
    delivery: &str,
    session: &str,
    d: J,
    guard_dir: &Path,
) -> Result<J> {
    let _lock = guard::locked(guard_dir)?;
    acknowledge_guard_state(root, s, delivery, session, d, guard_dir)
}

fn acknowledge_guard_state(
    root: &Path,
    s: J,
    delivery: &str,
    session: &str,
    d: J,
    guard_dir: &Path,
) -> Result<J> {
    let g = util::read_state_json(guard_dir, "state.json")?;
    acknowledge_receiver(root, s, delivery, session, d, guard_dir, g)
}

fn acknowledge_receiver(
    root: &Path,
    s: J,
    delivery: &str,
    session: &str,
    d: J,
    guard_dir: &Path,
    g: J,
) -> Result<J> {
    ensure_ack_receiver(&s, &g, session)?;
    acknowledge_marker(root, s, delivery, session, d, guard_dir, g)
}

fn ensure_ack_receiver(s: &J, g: &J, session: &str) -> Result<()> {
    match (
        guard::scope(g) == model::get(s, "receiver").cloned().unwrap_or(Value::Null),
        model::string(g, "session").as_deref() == Some(session),
    ) {
        (true, true) => Ok(()),
        _ => Err(queue_error("receiver changed before ACK")),
    }
}

fn acknowledge_marker(
    root: &Path,
    s: J,
    delivery: &str,
    session: &str,
    d: J,
    guard_dir: &Path,
    g: J,
) -> Result<J> {
    let marker = model::get(&g, "external_delivery").cloned();
    let expected = json!({
        "queue": root.display().to_string(),
        "delivery": delivery,
        "session": session
    });
    let Some(marker) = marker.filter(|value| model::truthy(Some(value))) else {
        return acknowledge_events(root, s, delivery, d, guard_dir, g);
    };
    if marker != expected {
        return Err(queue_error("another delivery owns receiver"));
    }
    acknowledge_events(root, s, delivery, d, guard_dir, g)
}

fn acknowledge_events(
    root: &Path,
    mut s: J,
    delivery: &str,
    d: J,
    guard_dir: &Path,
    g: J,
) -> Result<J> {
    acknowledge_event_ids(&mut s, &d);
    acknowledge_finish(root, s, delivery, d, guard_dir, g)
}

fn acknowledge_event_ids(s: &mut J, d: &J) {
    if let Some(ids) = model::array_field(d, "events") {
        acknowledge_ids(s, ids);
    }
}

fn acknowledge_ids(s: &mut J, ids: &[J]) {
    for event_id in ids {
        acknowledge_id(s, event_id);
    }
}

fn acknowledge_id(s: &mut J, event_id: &J) {
    if let Some(id) = event_id.as_str() {
        mark_event_acknowledged(s, id);
    }
}

fn mark_event_acknowledged(s: &mut J, id: &str) {
    if let Some(event) = model::get_mut(s, "events")
        .and_then(Value::as_object_mut)
        .and_then(|events| events.get_mut(id))
    {
        event["acknowledged"] = json!(true);
    }
}

fn log_acknowledged(delivery: &str, d: &J) {
    logging::log_event_fields(
        "delivery_acknowledged",
        "wait_queue",
        &format!("Delivery {delivery} acknowledged"),
        logging::Fields {
            metadata: Some(&json!({
                "delivery_id": delivery,
                "event_count": model::array_field(d, "events").map(Vec::len).unwrap_or(0)
            })),
            ..Default::default()
        },
    );
}

fn acknowledge_finish(
    root: &Path,
    mut s: J,
    delivery: &str,
    mut d: J,
    guard_dir: &Path,
    mut g: J,
) -> Result<J> {
    d["phase"] = json!("acknowledged");
    d["ack_at"] = json!(util::now());
    s["active"] = Value::Null;
    s["delivery_status"] = json!("acknowledged");
    log_acknowledged(delivery, &d);
    model::remove(&mut g, "external_delivery");
    acknowledge_save(root, s, delivery, d, guard_dir, g)
}

fn acknowledge_save(
    root: &Path,
    s: J,
    delivery: &str,
    d: J,
    guard_dir: &Path,
    mut g: J,
) -> Result<J> {
    guard::save(guard_dir, &mut g)?;
    acknowledge_store(root, s, delivery, d)
}

fn acknowledge_store(root: &Path, mut s: J, delivery: &str, d: J) -> Result<J> {
    s["deliveries"][delivery] = d;
    save(root, &mut s)?;
    Ok(s)
}

fn validate_resolution(resolution: &str, reason: &str) -> Result<()> {
    if !["received", "not-delivered"].contains(&resolution) {
        return Err(queue_error(
            "explicit inspected resolution and reason required",
        ));
    }
    validate_resolution_reason(reason)
}

fn validate_resolution_reason(reason: &str) -> Result<()> {
    match (reason.is_empty(), reason.chars().count() > 1000) {
        (false, false) => Ok(()),
        _ => Err(queue_error(
            "explicit inspected resolution and reason required",
        )),
    }
}

pub fn resolve(root: &Path, delivery: &str, resolution: &str, reason: &str) -> Result<J> {
    validate_resolution(resolution, reason)?;
    resolve_loaded(
        root,
        &DeliveryResolution {
            delivery,
            resolution,
            reason,
        },
    )
}

fn resolve_loaded(root: &Path, delivery_resolution: &DeliveryResolution<'_>) -> Result<J> {
    let s = load_queue(root)?;
    resolve_active(root, s, delivery_resolution)
}

fn resolve_active(root: &Path, s: J, delivery_resolution: &DeliveryResolution<'_>) -> Result<J> {
    ensure_active_matches(&s, delivery_resolution.delivery)?;
    resolve_record(root, s, delivery_resolution)
}

fn resolve_record(root: &Path, s: J, delivery_resolution: &DeliveryResolution<'_>) -> Result<J> {
    let d = delivery_record(&s, delivery_resolution.delivery)?;
    resolve_guard(root, s, delivery_resolution, d)
}

fn resolve_guard(
    root: &Path,
    s: J,
    delivery_resolution: &DeliveryResolution<'_>,
    d: J,
) -> Result<J> {
    let guard_dir = guard_root(&s)?;
    resolve_locked(root, s, delivery_resolution, d, &guard_dir)
}

fn resolve_locked(
    root: &Path,
    s: J,
    delivery_resolution: &DeliveryResolution<'_>,
    d: J,
    guard_dir: &Path,
) -> Result<J> {
    let _lock = guard::locked(guard_dir)?;
    resolve_guard_state(root, s, delivery_resolution, d, guard_dir)
}

fn resolve_guard_state(
    root: &Path,
    s: J,
    delivery_resolution: &DeliveryResolution<'_>,
    d: J,
    guard_dir: &Path,
) -> Result<J> {
    let g = util::read_state_json(guard_dir, "state.json")?;
    resolve_marker(root, s, delivery_resolution, d, guard_dir, g)
}

fn resolve_marker(
    root: &Path,
    s: J,
    delivery_resolution: &DeliveryResolution<'_>,
    d: J,
    guard_dir: &Path,
    g: J,
) -> Result<J> {
    let marker = model::get(&g, "external_delivery").cloned();
    let expected = json!({
        "queue": root.display().to_string(),
        "delivery": delivery_resolution.delivery,
        "session": d.get("session").cloned().unwrap_or(Value::Null)
    });
    let Some(marker) = marker.filter(|value| model::truthy(Some(value))) else {
        return resolve_events(root, s, delivery_resolution, d, guard_dir, g);
    };
    if marker != expected {
        return Err(queue_error("receiver marker belongs to another delivery"));
    }
    resolve_events(root, s, delivery_resolution, d, guard_dir, g)
}

fn resolve_events(
    root: &Path,
    mut s: J,
    delivery_resolution: &DeliveryResolution<'_>,
    d: J,
    guard_dir: &Path,
    g: J,
) -> Result<J> {
    acknowledge_resolved_events(&mut s, &d, delivery_resolution.resolution);
    resolve_finish(root, s, delivery_resolution, d, guard_dir, g)
}

fn acknowledge_resolved_events(s: &mut J, d: &J, resolution: &str) {
    if resolution != "received" {
        return;
    }
    acknowledge_event_ids(s, d);
}

fn resolve_finish(
    root: &Path,
    mut s: J,
    delivery_resolution: &DeliveryResolution<'_>,
    mut d: J,
    guard_dir: &Path,
    mut g: J,
) -> Result<J> {
    d["phase"] = json!("resolved");
    d["resolution"] = json!(delivery_resolution.resolution);
    d["reason"] = json!(delivery_resolution.reason);
    d["resolved_at"] = json!(util::now());
    s["active"] = Value::Null;
    s["notify"] = json!(false);
    s["delivery_status"] = json!("resolved_delivery_disabled");
    s["deliveries"][delivery_resolution.delivery] = d;
    model::remove(&mut g, "external_delivery");
    resolve_save(root, s, guard_dir, g)
}

fn resolve_save(root: &Path, s: J, guard_dir: &Path, mut g: J) -> Result<J> {
    guard::save(guard_dir, &mut g)?;
    resolve_store(root, s)
}

fn resolve_store(root: &Path, mut s: J) -> Result<J> {
    save(root, &mut s)?;
    Ok(s)
}

pub const DELIVERY_GUIDANCE: &[(&str, &str)] = &[
    (
        "disabled",
        "notifications are disabled; status is a board only",
    ),
    (
        "waiting_for_events",
        "no pending event; no model polling is needed",
    ),
    ("sending", "delivery is being submitted; do not resend it"),
    (
        "submitted",
        "delivery is submitted; wait for the exact receiver ACK",
    ),
    (
        "uncertain",
        "delivery is uncertain; inspect the exact receiver and delivery; do not resend",
    ),
    (
        "ack_overdue",
        "receiver ACK is overdue; inspect the receiver and delivery; do not resend",
    ),
    (
        "receiver_handoff_or_delivery",
        "receiver is handing off or has an external delivery; resolve or ACK it first",
    ),
    (
        "receiver_busy_or_blocked",
        "receiver is busy or blocked; inspect its active turn/approval; do not kill it",
    ),
    (
        "receiver_locked",
        "receiver lock is busy; retry one observation later; do not bypass the mutex",
    ),
    (
        "needs_attention",
        "delivery failed; inspect delivery_error and receiver identity; do not resend",
    ),
    (
        "acknowledged",
        "receipt confirmed; apply unapplied results before new dispatch",
    ),
    (
        "resolved_delivery_disabled",
        "delivery resolved; notifications are disabled",
    ),
];

pub fn delivery_diagnosis(s: &J) -> J {
    let status = model::string(s, "delivery_status").unwrap_or_else(|| "unknown".to_string());
    let guidance = DELIVERY_GUIDANCE
        .iter()
        .find(|(key, _)| *key == status)
        .map(|(_, guidance)| *guidance)
        .unwrap_or("inspect queue.json and receiver state; do not resend");
    json!({"status": status, "guidance": guidance})
}

pub fn diagnostics(s: &J, now: Option<f64>) -> Result<J> {
    Ok(json!({
        "delivery": delivery_diagnosis(s),
        "tasks": progress::diagnostics(s, now)?
    }))
}

pub fn summary(s: &J) -> String {
    let mut rows = Vec::new();
    if let Some(waits) = model::get(s, "waits").and_then(Value::as_object) {
        for w in waits.values() {
            let deadline = util::format_utc(model::number(w, "deadline").unwrap_or(0.0));
            let closed = if model::bool_or(w, "closed", false) {
                " | closed"
            } else {
                ""
            };
            rows.push(format!(
                "{}: {} | owner={} | {} | until={} | deadline={}{}",
                model::string(w, "id").unwrap_or_default(),
                model::string(w, "reason").unwrap_or_default(),
                model::string(w, "owner").unwrap_or_default(),
                model::string(w, "outcome").unwrap_or_default(),
                model::string(w, "condition").unwrap_or_default(),
                deadline,
                closed
            ));
        }
    }
    let has_tasks = model::get(s, "tasks")
        .and_then(Value::as_object)
        .map(|tasks| !tasks.is_empty())
        .unwrap_or(false);
    if has_tasks {
        let mut task_names: Vec<String> = model::get(s, "tasks")
            .and_then(Value::as_object)
            .map(|tasks| tasks.keys().cloned().collect())
            .unwrap_or_default();
        task_names.sort();
        for name in task_names {
            let Some(t) = model::dig(s, &["tasks", &name]) else {
                continue;
            };
            let next_kind = model::dig_str(t, &["next_action", "kind"]).unwrap_or_default();
            let pending = model::array_field(t, "pending").map(Vec::len).unwrap_or(0);
            rows.push(format!(
                "{}: {} | control={} | owner={} | version={} | next={} | unapplied={}",
                model::string(t, "task").unwrap_or_default(),
                model::string(t, "phase").unwrap_or_default(),
                model::string(t, "control").unwrap_or_default(),
                model::string(t, "owner").unwrap_or_default(),
                model::int(t, "state_version").unwrap_or(0),
                next_kind,
                pending
            ));
            if let Ok(d) = progress::diagnose(s, t, None) {
                rows.push(format!(
                    "  diagnosis={} | owner={} | {}",
                    model::string(&d, "classification").unwrap_or_default(),
                    model::string(&d, "owner").unwrap_or_default(),
                    model::string(&d, "reason").unwrap_or_default()
                ));
                rows.push(format!(
                    "  next={}",
                    model::string(&d, "next_step").unwrap_or_default()
                ));
            }
            if let Some(lane) = model::get(t, "lane").filter(|value| model::truthy(Some(value))) {
                let active = model::get(lane, "active").filter(|value| model::truthy(Some(value)));
                let batch_id = active
                    .and_then(|batch| model::string(batch, "id"))
                    .unwrap_or_else(|| "none".to_string());
                let batch_phase = active
                    .and_then(|batch| model::string(batch, "phase"))
                    .unwrap_or_else(|| "waiting".to_string());
                rows.push(format!(
                    "  batch={} | phase={} | pending={}/{}",
                    batch_id,
                    batch_phase,
                    model::array_field(lane, "pending")
                        .map(Vec::len)
                        .unwrap_or(0),
                    model::int(lane, "capacity").unwrap_or(0)
                ));
            }
        }
    } else {
        rows.push(
            "diagnosis=no_managed_tasks | next=register a concrete wait and task before dispatch"
                .to_string(),
        );
    }
    let delivery = delivery_diagnosis(s);
    rows.push(format!(
        "notification={}",
        model::string(&delivery, "status").unwrap_or_default()
    ));
    rows.push(format!(
        "  next={}",
        model::string(&delivery, "guidance").unwrap_or_default()
    ));
    rows.join("\n")
}

fn require_ready(
    root: &Path,
    session: Option<&str>,
    factory: &ReceiverFactory,
    message: &str,
) -> Result<()> {
    if !model::bool_or(
        &crate::runtime::readiness(root, session, factory)?,
        "active_wakeup",
        false,
    ) {
        return Err(queue_error(message));
    }
    Ok(())
}

type ProgressHandler = fn(&Path, &mut J, &J, Option<&str>, &ReceiverFactory, f64) -> Result<()>;

const PROGRESS_HANDLERS: &[(&str, ProgressHandler)] = &[
    ("task-add", progress_register),
    ("task-apply", progress_apply),
    ("task-begin", progress_begin),
    ("task-sent", progress_sent),
    ("task-control", progress_control),
    ("task-resume", progress_resume),
    ("task-release", progress_release),
];

fn progress_handler(action: &str) -> Option<ProgressHandler> {
    PROGRESS_HANDLERS
        .iter()
        .find(|(name, _)| *name == action)
        .map(|(_, handler)| *handler)
}

fn progress_action(
    root: &Path,
    s: &mut J,
    action: &str,
    spec: &J,
    session: Option<&str>,
    factory: &ReceiverFactory,
    now: f64,
) -> Result<()> {
    match progress_handler(action) {
        Some(handler) => handler(root, s, spec, session, factory, now),
        _ => Err(queue_error("unknown task action")),
    }
}

fn progress_register(
    _root: &Path,
    s: &mut J,
    spec: &J,
    _session: Option<&str>,
    _factory: &ReceiverFactory,
    now: f64,
) -> Result<()> {
    progress::register(s, spec, now).map(|_| ())
}

fn progress_apply(
    root: &Path,
    s: &mut J,
    spec: &J,
    session: Option<&str>,
    factory: &ReceiverFactory,
    now: f64,
) -> Result<()> {
    let next_action = progress::object_owned(model::get(spec, "next_action"), "next action")?;
    progress_apply_checked(root, s, spec, session, factory, now, &next_action)
}

fn progress_apply_checked(
    root: &Path,
    s: &mut J,
    spec: &J,
    session: Option<&str>,
    factory: &ReceiverFactory,
    now: f64,
    next_action: &J,
) -> Result<()> {
    if model::string(next_action, "kind").as_deref() == Some("dispatch") {
        return progress_apply_ready(root, s, spec, session, factory, now);
    }
    progress::apply(s, spec, now).map(|_| ())
}

fn progress_apply_ready(
    root: &Path,
    s: &mut J,
    spec: &J,
    session: Option<&str>,
    factory: &ReceiverFactory,
    now: f64,
) -> Result<()> {
    require_ready(
        root,
        session,
        factory,
        "dispatch intent requires current receiver notification readiness",
    )?;
    progress::apply(s, spec, now).map(|_| ())
}

fn progress_begin(
    root: &Path,
    s: &mut J,
    spec: &J,
    session: Option<&str>,
    factory: &ReceiverFactory,
    now: f64,
) -> Result<()> {
    require_ready(
        root,
        session,
        factory,
        "dispatch requires current receiver notification readiness",
    )?;
    progress::begin(
        s,
        model::get(spec, "task"),
        model::get(spec, "state_version"),
        model::get(spec, "operation"),
        now,
    )
    .map(|_| ())
}

fn progress_sent(
    _root: &Path,
    s: &mut J,
    spec: &J,
    _session: Option<&str>,
    _factory: &ReceiverFactory,
    now: f64,
) -> Result<()> {
    progress::sent(s, spec, now).map(|_| ())
}

fn progress_control(
    _root: &Path,
    s: &mut J,
    spec: &J,
    _session: Option<&str>,
    _factory: &ReceiverFactory,
    now: f64,
) -> Result<()> {
    progress::control(
        s,
        model::get(spec, "task"),
        model::get(spec, "state_version"),
        model::get(spec, "control"),
        model::get(spec, "reason"),
        now,
    )
    .map(|_| ())
}

fn progress_resume(
    _root: &Path,
    s: &mut J,
    spec: &J,
    _session: Option<&str>,
    _factory: &ReceiverFactory,
    now: f64,
) -> Result<()> {
    progress::resume(s, spec, now).map(|_| ())
}

fn progress_release(
    _root: &Path,
    s: &mut J,
    spec: &J,
    _session: Option<&str>,
    _factory: &ReceiverFactory,
    now: f64,
) -> Result<()> {
    progress::release(s, spec, now).map(|_| ())
}

pub fn task_action(
    root: &Path,
    action: &str,
    spec: &J,
    session: Option<&str>,
    factory: &ReceiverFactory,
) -> Result<J> {
    let _ = progress::object(spec, "task command spec")?;
    task_action_loaded(root, action, spec, session, factory)
}

fn task_action_loaded(
    root: &Path,
    action: &str,
    spec: &J,
    session: Option<&str>,
    factory: &ReceiverFactory,
) -> Result<J> {
    let mut s = load_queue(root)?;
    task_action_state(root, &mut s, action, spec, session, factory)
}

fn task_action_state(
    root: &Path,
    s: &mut J,
    action: &str,
    spec: &J,
    session: Option<&str>,
    factory: &ReceiverFactory,
) -> Result<J> {
    let now = util::now();
    task_dispatch(root, s, action, spec, session, factory, now)?;
    task_action_save(root, s)
}

fn task_action_save(root: &Path, s: &mut J) -> Result<J> {
    save(root, s)?;
    Ok(s.clone())
}

fn task_dispatch(
    root: &Path,
    s: &mut J,
    action: &str,
    spec: &J,
    session: Option<&str>,
    factory: &ReceiverFactory,
    now: f64,
) -> Result<()> {
    if action == "task-accept" {
        return task_accept(s, spec, session, now);
    }
    task_mutate(root, s, action, spec, session, factory, now)
}

fn task_accept(s: &mut J, spec: &J, session: Option<&str>, now: f64) -> Result<()> {
    let Some(session) = session else {
        return Err(queue_error(
            "task mutation requires the registered coordinator session",
        ));
    };
    progress::accept(s, spec, session, now).map(|_| ())
}

fn task_mutate(
    root: &Path,
    s: &mut J,
    action: &str,
    spec: &J,
    session: Option<&str>,
    factory: &ReceiverFactory,
    now: f64,
) -> Result<()> {
    crate::runtime::receiver_session(s, session)?;
    task_route_mutation(root, s, action, spec, session, factory, now)
}

fn task_route_mutation(
    root: &Path,
    s: &mut J,
    action: &str,
    spec: &J,
    session: Option<&str>,
    factory: &ReceiverFactory,
    now: f64,
) -> Result<()> {
    if action.starts_with("batch-") {
        return task_batch_mutation(root, s, action, spec, session, factory, now);
    }
    task_non_batch_mutation(root, s, action, spec, session, factory, now)
}

fn task_non_batch_mutation(
    root: &Path,
    s: &mut J,
    action: &str,
    spec: &J,
    session: Option<&str>,
    factory: &ReceiverFactory,
    now: f64,
) -> Result<()> {
    if action.starts_with("pr-") {
        return crate::pr::action(s, action, spec, now).map(|_| ());
    }
    progress_action(root, s, action, spec, session, factory, now)
}

fn task_batch_mutation(
    root: &Path,
    s: &mut J,
    action: &str,
    spec: &J,
    session: Option<&str>,
    factory: &ReceiverFactory,
    now: f64,
) -> Result<()> {
    if action == "batch-gate-begin" {
        return task_batch_gate(root, s, action, spec, session, factory, now);
    }
    task_batch_action(root, s, action, spec, now)
}

fn task_batch_gate(
    root: &Path,
    s: &mut J,
    action: &str,
    spec: &J,
    session: Option<&str>,
    factory: &ReceiverFactory,
    now: f64,
) -> Result<()> {
    require_ready(
        root,
        session,
        factory,
        "asynchronous batch gate requires current receiver notification readiness",
    )?;
    task_batch_action(root, s, action, spec, now)
}

fn task_batch_action(root: &Path, s: &mut J, action: &str, spec: &J, now: f64) -> Result<()> {
    let mut checkpoint = |state: &mut J| save(root, state);
    crate::batch::action(s, action, spec, now, &mut checkpoint).map(|_| ())
}

pub fn validate_watch(root: &Path) -> Result<J> {
    let state = load_queue(root)?;
    validate_watch_state(state)
}

fn validate_watch_state(state: J) -> Result<J> {
    ensure_watchable(&state)?;
    Ok(state)
}

fn ensure_watchable(state: &J) -> Result<()> {
    if model::bool_or(state, "stopped", false) {
        return Err(queue_error("stopped queue cannot be watched"));
    }
    Ok(())
}

/// Re-export for the CLI and queue summary.
pub fn diagnose(s: &J, t: &J, now: Option<f64>) -> Result<J> {
    progress::diagnose(s, t, now)
}
