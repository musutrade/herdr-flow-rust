//! Bind native submissions to the existing durable task result channel.
use super::*;
use crate::{progress, queue};

fn current_wait(queue: &J, wait_id: &str) -> Result<J> {
    ensure(
        !model::bool_or(queue, "stopped", false),
        "OpenCode result queue is stopped",
    )?;
    let wait = model::dig(queue, &["waits", wait_id])
        .cloned()
        .ok_or_else(|| FlowError::guard("OpenCode result wait is missing"))?;
    ensure(
        wait["kind"] == "signal" && !model::bool_or(&wait, "closed", false),
        "OpenCode result requires an open signal wait",
    )?;
    ensure(
        wait["producer_session"].is_null(),
        "OpenCode result wait cannot use Codex producer_session",
    )?;
    current_task(queue, &wait)?;
    Ok(wait)
}

fn current_task(queue: &J, wait: &J) -> Result<()> {
    let binding = progress::binding(wait)?;
    let name = required(wait, "task")?;
    let task = model::dig(queue, &["tasks", &name])
        .ok_or_else(|| FlowError::guard("OpenCode managed task is missing"))?;
    ensure(
        progress::same(task, &binding),
        "OpenCode task binding is no longer current",
    )?;
    ensure(
        task["control"] == "running" && task["phase"] != "completed",
        "OpenCode task is not running",
    )
}

fn queue_path(spec: &J) -> Result<PathBuf> {
    util::state_dir(Path::new(&required(spec, "queue_dir")?), false)
}

pub fn dispatch(root: &Path, spec: &J) -> Result<J> {
    let state = util::read_state_json(root, "state.json")?;
    let adapter = SessionAdapter::new(&state)?;
    let queue_root = queue_path(spec)?;
    let _lock = queue::locked(&queue_root, false)?;
    let queue = queue::load_queue(&queue_root)?;
    ensure_receiver_ready(&queue_root, &queue)?;
    let (prompt, context) = prepare_dispatch(&state, &queue_root, &queue, spec)?;
    let binding = context["binding"].clone();
    let timeout = model::number(spec, "timeout_seconds").unwrap_or(600.0);
    let id = adapter.submit(&prompt, Some(context), timeout)?;
    Ok(json!({"message_id":id,"session":state["session"],"phase":"submitted","binding":binding}))
}

fn prepare_dispatch(state: &J, queue_root: &Path, queue: &J, spec: &J) -> Result<(String, J)> {
    let wait_id = required(spec, "wait")?;
    let wait = current_wait(queue, &wait_id)?;
    let binding = progress::binding(&wait)?;
    verify_candidate(state, &binding)?;
    let prompt = util::read_text(Path::new(&required(spec, "prompt_file")?))?;
    let fingerprint = util::fingerprint(Path::new(&required(state, "cwd")?))?;
    let context = json!({"queue_dir":queue_root,"queue_id":queue["id"],"wait":wait_id,"binding":binding,
        "source_fingerprint":fingerprint,"producer_identity":{"adapter":"opencode","session":state["session"],"model":state["model"],"cwd":state["cwd"]}});
    Ok((prompt, context))
}

pub fn collect(root: &Path) -> Result<J> {
    let state = util::read_state_json(root, "state.json")?;
    let adapter = SessionAdapter::new(&state)?;
    adapter.verify_binding()?;
    let journal = adapter
        .reconcile()?
        .ok_or_else(|| FlowError::guard("no OpenCode submission to collect"))?;
    if !terminal(&journal) {
        return Ok(journal);
    }
    if journal["context"].is_null() {
        return Ok(journal);
    }
    collect_result(root, &adapter, journal)
}

fn collect_result(root: &Path, adapter: &SessionAdapter, journal: J) -> Result<J> {
    let context = journal["context"].clone();
    let queue_root = queue_path(&context)?;
    let _queue_lock = queue::locked(&queue_root, false)?;
    let _submission_lock = submission_lock(root)?;
    verify_current_submission(root, &journal)?;
    let queue = queue::load_queue(&queue_root)?;
    verify_collection(adapter, &queue, &context)?;
    publish_result(root, adapter, journal, &queue_root)
}

fn verify_current_submission(root: &Path, journal: &J) -> Result<()> {
    let current =
        load_submission(root)?.ok_or_else(|| FlowError::guard("submission disappeared"))?;
    ensure(
        current["message_id"] == journal["message_id"],
        "OpenCode submission changed before result collection",
    )
}

fn verify_collection(adapter: &SessionAdapter, queue: &J, context: &J) -> Result<()> {
    ensure(
        queue["id"] == context["queue_id"],
        "OpenCode result queue identity changed",
    )?;
    let wait_id = required(context, "wait")?;
    let wait = current_wait(queue, &wait_id)?;
    ensure(
        progress::binding(&wait)? == context["binding"],
        "OpenCode result assignment/candidate changed",
    )?;
    verify_candidate(&adapter.state, &context["binding"])?;
    adapter.verify_binding()?;
    Ok(())
}

fn publish_result(
    root: &Path,
    adapter: &SessionAdapter,
    mut journal: J,
    queue_root: &Path,
) -> Result<J> {
    journal["source_fingerprint_after"] = json!(util::fingerprint(Path::new(&required(
        &adapter.state,
        "cwd"
    )?))?);
    let wait_id = required(&journal["context"], "wait")?;
    let outcome = result_outcome(&journal)?.to_string();
    journal["outcome"] = json!(outcome);
    // Preserve the report before its signal. Restarting collection verifies the
    // same result without resubmitting a prompt.
    save_submission(root, &journal)?;
    let id = required(&journal, "message_id")?;
    let evidence = root.join("submissions").join(format!("{id}.json"));
    let summary = format!(
        "OpenCode {} finished: {outcome}; inspect the bound report before accepting findings",
        adapter.session()
    );
    queue::signal(
        queue_root,
        &wait_id,
        &outcome,
        &summary,
        Some(&evidence.display().to_string()),
    )?;
    journal["phase"] = json!("signalled");
    save_submission(root, &journal)?;
    Ok(journal)
}

fn result_outcome(journal: &J) -> Result<&str> {
    let outcome = journal
        .get("outcome")
        .and_then(Value::as_str)
        .or_else(|| journal.get("phase").and_then(Value::as_str))
        .unwrap_or_default();
    ensure(
        ["complete", "failed", "decision"].contains(&outcome),
        "OpenCode result outcome is unresolved",
    )?;
    Ok(outcome)
}

fn verify_candidate(state: &J, binding: &J) -> Result<()> {
    let cwd = PathBuf::from(required(state, "cwd")?);
    let head = crate::pr::git(&cwd, &["rev-parse", "HEAD"])?;
    ensure(
        head == required(binding, "candidate")?,
        "OpenCode workspace HEAD no longer matches the bound candidate",
    )
}

fn ensure_receiver_ready(root: &Path, queue: &J) -> Result<()> {
    let guard_dir = PathBuf::from(required(queue, "guard_dir")?);
    let guard = util::read_state_json(&guard_dir, "state.json")?;
    let session = required(&guard, "session")?;
    let readiness =
        crate::runtime::readiness(root, Some(&session), &crate::herdr::receiver_factory)?;
    ensure(
        readiness["active_wakeup"] == true,
        "coordinator notification receiver is not ready before OpenCode dispatch",
    )
}
