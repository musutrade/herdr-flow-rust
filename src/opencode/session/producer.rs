//! Bind native submissions to the existing durable task result channel.
use super::*;
use crate::{progress, queue};

fn current_wait(queue: &J, wait_id: &str, state: &J) -> Result<J> {
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
    verify_producer(&wait, state)?;
    current_task(queue, &wait)?;
    Ok(wait)
}

fn verify_producer(wait: &J, state: &J) -> Result<()> {
    if let Some(producer) = crate::identity::producer(wait)? {
        ensure(
            producer == json!({"adapter":"opencode","session":state["session"]}),
            "OpenCode producer identity mismatch",
        )?;
    } else {
        ensure(
            wait["lineage"].is_null(),
            "linked activity requires a producer identity",
        )?;
    }
    Ok(())
}

fn current_task(queue: &J, wait: &J) -> Result<()> {
    let binding = progress::binding(wait)?;
    let name = required(wait, "task")?;
    let task = model::dig(queue, &["tasks", &name])
        .ok_or_else(|| FlowError::guard("OpenCode managed task is missing"))?;
    progress::lineage::ensure_current(queue, task)?;
    progress::claim::validate(queue, task)?;
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
    let (wait, binding) = dispatch_binding(state, queue, &wait_id)?;
    let prompt = dispatch_prompt(spec)?;
    let fingerprint = util::fingerprint(Path::new(&required(state, "cwd")?))?;
    let mut context = json!({"queue_dir":queue_root,"queue_id":queue["id"],"wait":wait_id,"binding":binding,
        "source_fingerprint":fingerprint,"producer_identity":{"adapter":"opencode","session":state["session"],"model":state["model"],"cwd":state["cwd"]}});
    bind_dispatch_owner(queue, &wait, spec, &mut context)?;
    Ok((prompt, context))
}

fn dispatch_binding(state: &J, queue: &J, wait_id: &str) -> Result<(J, J)> {
    let wait = current_wait(queue, wait_id, state)?;
    progress::claim::execution(queue, &queue["tasks"][wait["task"].as_str().unwrap_or("")])?;
    let binding = progress::binding(&wait)?;
    verify_candidate(state, &binding)?;
    Ok((wait, binding))
}

fn dispatch_prompt(spec: &J) -> Result<String> {
    util::read_text(Path::new(&required(spec, "prompt_file")?))
}

fn bind_dispatch_owner(queue: &J, wait: &J, spec: &J, context: &mut J) -> Result<()> {
    if !queue["lifecycle"].is_null() {
        let name = required(wait, "task")?;
        let owner = crate::lifecycle::task_owner(queue, &name)?
            .ok_or_else(|| FlowError::guard("isolated submission requires bound owner"))?;
        if !spec["lifecycle_owner"].is_null() {
            ensure(
                spec["lifecycle_owner"] == serde_json::to_value(&owner)?,
                "submission owner changed from creation",
            )?;
        }
        context["lifecycle_owner"] = serde_json::to_value(owner)?;
    }
    Ok(())
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
    let _permit = crate::lifecycle::context_permit(&context)?;
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
    let wait = current_wait(queue, &wait_id, &adapter.state)?;
    ensure(
        progress::binding(&wait)? == context["binding"],
        "OpenCode result assignment/candidate changed",
    )?;
    verify_collection_source(adapter, context)
}
fn verify_collection_source(adapter: &SessionAdapter, context: &J) -> Result<()> {
    if context["binding"]["lineage"]["role"] != "development" {
        verify_candidate(&adapter.state, &context["binding"])?;
    }
    ensure(
        context["producer_identity"]
            == json!({"adapter":"opencode","session":adapter.state["session"],"model":adapter.state["model"],"cwd":adapter.state["cwd"]}),
        "OpenCode result producer binding changed",
    )?;
    adapter.verify_binding()?;
    Ok(())
}

fn publish_result(
    root: &Path,
    adapter: &SessionAdapter,
    mut journal: J,
    queue_root: &Path,
) -> Result<J> {
    prepare_output(adapter, &mut journal)?;
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
    collect_verdict(adapter, queue_root, &wait_id, &outcome)?;
    signal_report(
        queue_root,
        adapter,
        &journal["context"],
        &outcome,
        &summary,
        &evidence,
    )?;
    journal["phase"] = json!("signalled");
    save_submission(root, &journal)?;
    Ok(journal)
}

fn collect_verdict(
    adapter: &SessionAdapter,
    queue_root: &Path,
    wait_id: &str,
    outcome: &str,
) -> Result<()> {
    progress::acceptance::native::collect(
        queue_root,
        wait_id,
        &json!({"adapter":"opencode","session":adapter.state["session"]}),
        Path::new(&required(&adapter.state, "cwd")?),
        outcome,
    )?;
    Ok(())
}

fn prepare_output(adapter: &SessionAdapter, journal: &mut J) -> Result<()> {
    if journal["source_fingerprint_after"].is_null() {
        journal["source_fingerprint_after"] = json!(util::fingerprint(Path::new(&required(
            &adapter.state,
            "cwd"
        )?))?);
        capture_output(&adapter.state, journal)?;
    }
    Ok(())
}

fn signal_report(
    queue_root: &Path,
    adapter: &SessionAdapter,
    context: &J,
    outcome: &str,
    summary: &str,
    evidence: &Path,
) -> Result<()> {
    let wait_id = &required(context, "wait")?;
    let wait = queue::load_queue(queue_root)?["waits"][wait_id].clone();
    if crate::identity::producer(&wait)?.is_some() {
        queue::signal_native(
            queue_root,
            &json!({"adapter":"opencode","session":adapter.state["session"]}),
            &json!({"wait":wait_id,"outcome":outcome,"summary":summary,"evidence":evidence,"lifecycle_owner":context["lifecycle_owner"]}),
        )?;
    } else {
        queue::signal(
            queue_root,
            wait_id,
            outcome,
            summary,
            Some(&evidence.display().to_string()),
        )?;
    }
    Ok(())
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
    )?;
    let diff = crate::pr::git(&cwd, &["diff", "HEAD", "--"])?;
    ensure(
        diff.is_empty(),
        "OpenCode bound candidate tracked contents changed",
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

fn capture_output(state: &J, journal: &mut J) -> Result<()> {
    let binding = &journal["context"]["binding"];
    if binding["lineage"]["role"] != "development" {
        return Ok(());
    }
    ensure(
        binding["artifact_adapter"] == "git",
        "OpenCode development currently requires git artifacts",
    )?;
    let cwd = required(state, "cwd")?;
    let head = crate::pr::git(Path::new(&cwd), &["rev-parse", "HEAD"])?;
    let output = json!({"adapter":"git","id":head,"location":cwd,"input":binding["candidate"]});
    use crate::progress::artifact::ArtifactAdapter;
    crate::progress::artifact::LocalArtifacts.verify(&output)?;
    journal["output"] = output;
    Ok(())
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    #[test]
    fn dispatch_carries_exact_owner_and_rejects_creation_context_replacement() {
        let q = json!({"id":"queue","lifecycle":{"queue_root":"/fixture/queue"},
            "tasks":{"task":{"task":"task","epoch":1,"environment":{"instance_id":"instance"}}}});
        let wait = json!({"task":"task"});
        let owner = json!({"queue_root":"/fixture/queue","queue_id":"queue","task_id":"task","epoch":1,"instance_id":"instance"});
        let mut context = json!({});
        bind_dispatch_owner(&q, &wait, &json!({}), &mut context).unwrap();
        assert_eq!(context["lifecycle_owner"], owner);
        bind_dispatch_owner(&q, &wait, &json!({"lifecycle_owner":owner}), &mut context).unwrap();
        let before = context.clone();
        let mut bad = owner.clone();
        bad["epoch"] = json!(2);
        assert!(
            bind_dispatch_owner(&q, &wait, &json!({"lifecycle_owner":bad}), &mut context).is_err()
        );
        assert_eq!(context, before);
        assert!(
            bind_dispatch_owner(&q, &json!({"task":"unknown"}), &json!({}), &mut context).is_err()
        );
    }
}
