//! Bounded publication steps. No remote request runs under the queue lock.
//! Each submitting step records uncertainty first and is attempted at most once.
use super::*;
use transport::Endpoint;

pub(super) fn advance(root: &Path, context: &J, operation: &mut J) -> Result<()> {
    let remote = transport::Remote::new(&context["request"]["policy"]);
    advance_with(root, context, operation, &remote)
}

pub(super) fn advance_with(
    root: &Path,
    context: &J,
    operation: &mut J,
    remote: &dyn Endpoint,
) -> Result<()> {
    if operation["retry_observation_at"]
        .as_f64()
        .is_some_and(|at| at > util::now())
    {
        return Ok(());
    }
    match operation["phase"].as_str() {
        Some("planned") => ask(
            root,
            context,
            operation,
            "planning",
            "publish",
            &context["request"],
        ),
        Some("planning") => collect(root, context, operation, "reserving"),
        Some("reserving") => reserve(root, context, operation, remote),
        Some("pushing") => push(root, context, operation, remote),
        Some("creating") => create(root, context, operation, remote),
        Some("waiting_ci") => wait_ci(root, context, operation, remote),
        _ => finish(root, context, operation, remote),
    }
}

fn finish(root: &Path, context: &J, operation: &mut J, remote: &dyn Endpoint) -> Result<()> {
    match operation["phase"].as_str() {
        Some("merge_planning") => collect(root, context, operation, "merging"),
        Some("merging") => merge(root, context, operation, remote),
        Some("confirming") => confirm(root, context, operation, remote),
        Some("complete" | "attention") => Ok(()),
        _ => Err(FlowError::guard(
            "publication startup or phase is unresolved; inspect without replay",
        )),
    }
}

fn ask(
    root: &Path,
    context: &J,
    operation: &mut J,
    phase: &str,
    action: &str,
    evidence: &J,
) -> Result<()> {
    operation["phase"] = json!(phase);
    agent::request(root, context, operation, action, evidence)
}

fn collect(root: &Path, context: &J, operation: &mut J, next: &str) -> Result<()> {
    let Some(result) = agent::collect(root, context, operation)? else {
        return Ok(());
    };
    let action = required(&operation["model_turn"], "action")?;
    operation["requests"][action] = result.clone();
    operation["phase"] = json!(if result["action"] == "report_blocker" {
        "attention"
    } else {
        next
    });
    Ok(())
}

fn state(context: &J) -> Result<(Owner, J)> {
    let owner: Owner = serde_json::from_value(context["owner"].clone())?;
    let state = snapshot(&owner)?;
    child(&state, context)?;
    Ok((owner, state))
}

fn task_spec(task: &J) -> J {
    json!({"task":task["task"],"state_version":task["state_version"]})
}

fn admit(context: &J, expected: &J) -> Result<()> {
    let (_, current) = state(context)?;
    ensure(
        child(&current, context)?["pr"] == *expected,
        "publication record changed before submission",
    )
}

fn reserve(root: &Path, context: &J, operation: &mut J, remote: &dyn Endpoint) -> Result<()> {
    let (owner, before) = state(context)?;
    reserve_pr(context, &before, remote)?;
    let permit = crate::lifecycle::write_permit(&owner)?;
    let mut env = crate::task_env::instance::allocation::restore(&owner, &permit)?;
    crate::delivery::publication::reserve_remote(&mut env)?;
    operation["phase"] = json!("pushing");
    save(root, "operation.json", operation)
}

fn reserve_pr(context: &J, before: &J, remote: &dyn Endpoint) -> Result<()> {
    let task = child(before, context)?;
    if task["pr"]["publication"]["phase"] == "prepared" {
        let mut staged = before.clone();
        let mut spec = task_spec(task);
        spec["authorization"] = context["request"]["publication_authorization"].clone();
        let updated =
            crate::pr::publication_begin_with_api(&mut staged, &spec, util::now(), remote)?;
        commit(context, before, &updated, true)?;
    } else {
        ensure(
            task["pr"]["publication"]["phase"] == "sending",
            "publication reservation has changed",
        )?;
    }
    Ok(())
}

/// Durable sending is deliberately not rolled back on any transport error.
fn submit(
    root: &Path,
    operation: &mut J,
    key: &str,
    execute: impl FnOnce() -> Result<()>,
) -> Result<()> {
    if !operation["submissions"][key].is_null() {
        return Ok(());
    }
    operation["submissions"][key] = json!({"phase":"sending"});
    save(root, "operation.json", operation)?;
    let result = execute();
    operation["submissions"][key] = match result {
        Ok(()) => json!({"phase":"returned"}),
        Err(error) => json!({"phase":"unknown","error":error.to_string()}),
    };
    save(root, "operation.json", operation)
}

fn push(root: &Path, context: &J, operation: &mut J, remote: &dyn Endpoint) -> Result<()> {
    let (_, state) = state(context)?;
    let record = &child(&state, context)?["pr"];
    admit(context, record)?;
    submit(root, operation, "push", || remote.push(record))?;
    let branch = remote.branch(record)?;
    ensure(
        branch["object"]["sha"] == record["head"],
        "original push is not confirmed at its exact branch; never replay",
    )?;
    operation["pushed"] = branch;
    operation["phase"] = json!("creating");
    Ok(())
}

fn create(root: &Path, context: &J, operation: &mut J, remote: &dyn Endpoint) -> Result<()> {
    let (_, state) = state(context)?;
    let record = &child(&state, context)?["pr"];
    let request = &context["request"];
    admit(context, record)?;
    create_once(root, operation, remote, record, request)?;
    let task = reconcile(context, remote)?;
    ensure(
        task["pr"]["number"].as_u64().is_some_and(|n| n > 0),
        "original PR creation is unconfirmed; never create a replacement",
    )?;
    remote.verify_open(&task["pr"])?;
    operation["phase"] = json!("waiting_ci");
    Ok(())
}

fn create_once(
    root: &Path,
    operation: &mut J,
    remote: &dyn Endpoint,
    record: &J,
    request: &J,
) -> Result<()> {
    let title = required(request, "title")?;
    let body = required(request, "body")?;
    submit(root, operation, "create", || {
        remote.create(record, &title, &body)
    })
}

fn reconcile(context: &J, remote: &dyn Endpoint) -> Result<J> {
    let (_, before) = state(context)?;
    let spec = task_spec(child(&before, context)?);
    let mut staged = before.clone();
    let updated = crate::pr::reconcile(&mut staged, &spec, util::now(), Some(remote))?;
    commit(context, &before, &updated, false)?;
    Ok(updated)
}

fn wait_ci(root: &Path, context: &J, operation: &mut J, remote: &dyn Endpoint) -> Result<()> {
    if operation["next_observation"]
        .as_f64()
        .is_some_and(|n| n > util::now())
    {
        return Ok(());
    }
    let observed = refresh_ci(root, context, remote)?;
    operation["ci"] = observed["pr"]["observation"].clone();
    if observed["pr"]["phase"] == "checks_satisfied" {
        let evidence =
            json!({"pr":observed["pr"],"authorization":context["request"]["merge_authorization"]});
        return ask(
            root,
            context,
            operation,
            "merge_planning",
            "merge",
            &evidence,
        );
    }
    ci_pending(&observed["pr"])?;
    operation["next_observation"] = json!(util::now() + 30.0);
    Ok(())
}

fn ci_pending(pr: &J) -> Result<()> {
    ensure(
        matches!(pr["phase"].as_str(), Some("awaiting_ci" | "checks_pending")),
        "publication requires a decision about CI or changed remote state",
    )
}

fn ci_config(context: &J, task: &J) -> J {
    json!({"version":1,"host":task["pr"]["host"],"repo":task["pr"]["repo"],
        "task":task["task"],"epoch":task["epoch"],"pr":task["pr"]["number"],"sha":task["pr"]["head"],
        "workflows":context["request"]["policy"]["workflows"]})
}

fn observe_ci(root: &Path, config: &J, remote: &dyn Endpoint) -> Result<J> {
    crate::ci::validate(config)?;
    let result = crate::ci::observe(config, remote)?;
    let hash = crate::ci::config_hash(config);
    util::ensure_private_dir(root, true)?;
    // Owned publication observer uses the same schema and calculations as CI,
    // with its existing lifecycle permit instead of the unbound CI CLI writer.
    save(
        root,
        "identity.json",
        &json!({"config_hash":hash,"config":config}),
    )?;
    let mut status = result;
    status["config_hash"] = json!(hash);
    status["observed_at"] = json!(util::now());
    save(root, "status.json", &status)?;
    Ok(status)
}

fn refresh_ci(root: &Path, context: &J, remote: &dyn Endpoint) -> Result<J> {
    let (_, before) = state(context)?;
    let task = child(&before, context)?;
    remote.verify_open(&task["pr"])?;
    let config = ci_config(context, task);
    let source = root.join("ci");
    observe_ci(&source, &config, remote)?;
    let mut spec = task_spec(task);
    spec["source"] = json!(source);
    let mut staged = before.clone();
    let updated = crate::pr::observe(&mut staged, &spec, util::now(), Some(remote))?;
    commit(context, &before, &updated, false)?;
    Ok(updated)
}

fn merge(root: &Path, context: &J, operation: &mut J, remote: &dyn Endpoint) -> Result<()> {
    if operation["submissions"]["merge"].is_null() {
        let task = refresh_ci(root, context, remote)?;
        ensure(
            task["pr"]["phase"] == "checks_satisfied",
            "fresh exact-head CI is not successful; merge not submitted",
        )?;
        operation["merge_ci"] = task["pr"]["observation"].clone();
        merge_ready(context, &task["pr"], remote)?;
        submit(root, operation, "merge", || remote.merge(&task["pr"]))?;
    }
    operation["phase"] = json!("confirming");
    Ok(())
}

fn merge_ready(context: &J, record: &J, remote: &dyn Endpoint) -> Result<()> {
    let pull = remote.verify_open(record)?;
    ensure(
        pull["mergeable"] == true,
        "PR mergeability is not confirmed",
    )?;
    admit(context, record)
}

fn confirm(root: &Path, context: &J, operation: &mut J, remote: &dyn Endpoint) -> Result<()> {
    let task = reconcile(context, remote)?;
    ensure(
        task["pr"]["phase"] == "merged",
        "original merge is not confirmed; do not resubmit",
    )?;
    ensure(
        operation["merge_ci"]["state"] == "success",
        "saved successful premerge CI is missing",
    )?;
    if !landing_ci(root, context, operation, &task, remote)? {
        return Ok(());
    }
    let result = confirm_delivery(context, remote)?;
    confirmed_receipt(&result, &task)?;
    operation["receipt"] = result["receipt"].clone();
    // Actual delivery is a host fact. A final model turn must not withhold or
    // downgrade it into a nonterminal decision that blocks stage completion.
    operation["phase"] = json!("complete");
    Ok(())
}

fn confirm_delivery(context: &J, remote: &dyn Endpoint) -> Result<J> {
    let owner: Owner = serde_json::from_value(context["owner"].clone())?;
    let permit = crate::lifecycle::write_permit(&owner)?;
    let mut env = crate::task_env::instance::allocation::restore(&owner, &permit)?;
    crate::delivery::publication::confirm_remote(&mut env, &owner.queue_root, remote)
}

fn confirmed_receipt(result: &J, task: &J) -> Result<()> {
    if result["phase"] == "unknown" {
        return Err(FlowError::new(
            crate::ErrorKind::Transient,
            "fresh landing observation remains unknown; retain original delivery",
        ));
    }
    ensure(
        result["phase"] == "confirmed",
        "fresh remote landing is unconfirmed; no completion receipt",
    )?;
    ensure(
        crate::progress::lineage::publication::receipt_matches(
            &result["receipt"],
            &task["pr"]["landing"],
        ),
        "confirmed receipt does not match the original PR landing",
    )
}

fn landing_ci(
    root: &Path,
    context: &J,
    operation: &mut J,
    task: &J,
    remote: &dyn Endpoint,
) -> Result<bool> {
    let workflows = &context["request"]["policy"]["landing_workflows"];
    if workflows.as_array().is_none_or(Vec::is_empty) {
        return Ok(true);
    }
    if operation["next_landing_observation"]
        .as_f64()
        .is_some_and(|n| n > util::now())
    {
        return Ok(false);
    }
    let mut config = ci_config(context, task);
    config["sha"] = task["pr"]["landing"]["commit"].clone();
    config["pr"] = J::Null;
    config["workflows"] = workflows.clone();
    let observed = observe_ci(&root.join("landing-ci"), &config, remote)?;
    operation["landing_ci"] = observed.clone();
    if observed["state"] == "success" {
        return Ok(true);
    }
    ensure(
        observed["state"] == "pending",
        "landing CI needs a decision; keep the confirmed remote fact",
    )?;
    operation["next_landing_observation"] = json!(util::now() + 30.0);
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    #[test]
    fn fresh_unknown_or_mismatched_receipt_cannot_be_reported_complete() {
        let landing =
            json!({"head":"h","commit":"c","repo":"owner/repo","base_ref":"main","number":7});
        let task = json!({"pr":{"landing":landing}});
        for phase in ["unknown", "failed", "absent"] {
            assert!(confirmed_receipt(&json!({"phase":phase,"receipt":landing}), &task).is_err());
        }
        assert!(confirmed_receipt(&json!({"phase":"confirmed","receipt":{}}), &task).is_err());
        let mut receipt = json!({"phase":"confirmed","receipt":landing});
        confirmed_receipt(&receipt, &task).unwrap();
        receipt["receipt"]["commit"] = json!("different");
        assert!(confirmed_receipt(&receipt, &task).is_err());
    }
    #[test]
    fn unknown_or_interrupted_submission_never_replays() {
        let root = util::fs::test_tempdir();
        let calls = Cell::new(0);
        let mut operation = json!({"phase":"pushing"});
        submit(root.path(), &mut operation, "push", || {
            calls.set(calls.get() + 1);
            Err(FlowError::guard("lost response after remote effect"))
        })
        .unwrap();
        assert_eq!(calls.get(), 1);
        assert_eq!(operation["submissions"]["push"]["phase"], "unknown");
        submit(root.path(), &mut operation, "push", || {
            calls.set(calls.get() + 1);
            Ok(())
        })
        .unwrap();
        operation["submissions"]["create"] = json!({"phase":"sending"});
        submit(root.path(), &mut operation, "create", || {
            calls.set(calls.get() + 1);
            Ok(())
        })
        .unwrap();
        assert_eq!(calls.get(), 1);
        assert!(operation["receipt"].is_null());
    }
    #[test]
    fn successful_command_still_has_no_delivery_receipt() {
        let root = util::fs::test_tempdir();
        let mut operation = json!({"phase":"creating"});
        submit(root.path(), &mut operation, "create", || Ok(())).unwrap();
        assert_eq!(operation["submissions"]["create"]["phase"], "returned");
        assert!(operation["receipt"].is_null());
        assert_eq!(read(root.path(), "operation.json").unwrap(), operation);
    }
}
