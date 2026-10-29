use super::*;

/// At most one worker per watcher tick. EX is released between attempts; a
/// native wait/error never keeps the control queue locked or resubmits delivery.
pub(super) fn drive(
    env: &mut TaskEnvironment,
    plan: &mut Plan,
    permit: &crate::lifecycle::Permit,
) -> Result<()> {
    selection::no_recovery_incident(env)?;
    if let Some(index) = plan
        .entries
        .iter()
        .position(|entry| entry.phase != "complete")
    {
        validate_entry(env, &plan.entries[index])?;
        let result = retire(env, plan, index, permit);
        // A close/delete may have updated an obligation before returning an
        // observation error. Persist it for both retry and terminal takeover.
        save(env, plan)?;
        result?;
    }
    if plan.entries.iter().all(|entry| entry.phase == "complete") {
        plan.phase = "complete".into();
        save(env, plan)?;
    }
    Ok(())
}

fn validate_entry(env: &TaskEnvironment, entry: &Entry) -> Result<()> {
    validate_runtime(env, entry)?;
    validate_presentation(entry)?;
    let (_, current_frontends) = tui::retirement_snapshot(env, &entry.root)?;
    ensure(
        current_frontends == entry.frontends,
        "stage frontend process identities changed",
    )?;
    ensure(
        !entry.sessions.is_empty() && entry.sessions.len() <= 128,
        "retirement session obligations changed",
    )
}

fn validate_runtime(env: &TaskEnvironment, entry: &Entry) -> Result<()> {
    let assignment = required(&entry.binding, "assignment")?;
    validate_assignment(&assignment)?;
    ensure(
        entry.root == env.temp_root.join("rt").join(assignment),
        "retirement runtime path changed",
    )?;
    crate::task_env::validate_path(env, &entry.root)?;
    ensure(
        read(&entry.root, "runtime.json")? == entry.runtime,
        "frozen retirement runtime changed",
    )?;
    ensure(
        read(&entry.root, "creation.json")?["packet"]["binding"] == entry.binding,
        "retirement assignment changed",
    )
}

fn validate_presentation(entry: &Entry) -> Result<()> {
    let current = util::read_state_json_bounded_if_exists(&entry.root, "presentation.json", LIMIT)?;
    let mut expected = entry.presentation.clone();
    let mut actual = current;
    if entry.phase != "planned" {
        for record in [&mut expected, &mut actual].into_iter().flatten() {
            let object = record
                .as_object_mut()
                .ok_or_else(|| FlowError::guard("presentation binding is not an object"))?;
            object.remove("phase");
            object.remove("persisted_panes");
        }
    }
    ensure(
        actual == expected,
        "stage presentation changed; preserve the current frontend",
    )
}

fn retire(
    env: &mut TaskEnvironment,
    plan: &mut Plan,
    index: usize,
    permit: &crate::lifecycle::Permit,
) -> Result<()> {
    if plan.entries[index].phase == "planned" {
        begin(env, plan, index)?;
        return pending_step();
    }
    close_drained_presentation(env, plan, index)?;
    if plan.entries[index].phase == "retiring" {
        stop_transport(env, plan, index, permit)?;
        return pending_step();
    }
    retire_sessions(env, plan, index)?;
    plan.entries[index].phase = "complete".into();
    save(env, plan)?;
    marker(plan, index)
}

fn close_drained_presentation(
    env: &mut TaskEnvironment,
    plan: &mut Plan,
    index: usize,
) -> Result<()> {
    // The fence must precede the drain/idle proof. Existing approvals and input
    // stay reachable until both admitted native work and upstream activity end.
    marker(plan, index)?;
    if plan.entries[index].phase == "retiring" {
        require_presentation_drained(&plan.entries[index])?;
    }
    close_presentation(env, plan, index)?;
    require_frontends_gone(&plan.entries[index])
}

fn require_presentation_drained(entry: &Entry) -> Result<()> {
    if let Some(record) = &entry.presentation {
        tui::require_retirement_drained(record)?;
        idle(&entry.sessions[0])?;
    }
    Ok(())
}

fn stop_transport(
    env: &mut TaskEnvironment,
    plan: &mut Plan,
    index: usize,
    permit: &crate::lifecycle::Permit,
) -> Result<()> {
    idle(&plan.entries[index].sessions[0])?;
    crate::task_env::stop_registered_broker(env, &plan.entries[index].broker, permit)?;
    plan.entries[index].phase = "sessions".into();
    save(env, plan)
}

fn pending_step() -> Result<()> {
    Err(FlowError::new(
        crate::ErrorKind::StateBusy,
        "stage resources await the next bounded cleanup tick",
    ))
}

fn begin(env: &mut TaskEnvironment, plan: &mut Plan, index: usize) -> Result<()> {
    idle(&plan.entries[index].sessions[0])?;
    if let Some(record) = &plan.entries[index].presentation {
        plan.entries[index].presentation = Some(tui::capture_retirement(record)?);
    }
    plan.entries[index].phase = "retiring".into();
    save(env, plan)?;
    marker(plan, index)
}

fn marker(plan: &Plan, index: usize) -> Result<()> {
    let entry = &plan.entries[index];
    let marker = json!({"owner":plan.owner,"delivery":plan.delivery,"binding":entry.binding,
        "phase":if entry.phase == "complete" {"retired"} else {"retiring"}});
    if let Some(old) = util::read_state_json_bounded_if_exists(&entry.root, MARKER, LIMIT)? {
        ensure(
            old["owner"] == marker["owner"]
                && old["delivery"] == marker["delivery"]
                && old["binding"] == marker["binding"],
            "runtime retirement marker changed",
        )?;
    }
    util::atomic_write_json(&entry.root.join(MARKER), &marker)
}

fn close_presentation(env: &mut TaskEnvironment, plan: &mut Plan, index: usize) -> Result<()> {
    // Reassert the fence after a crash between the plan and marker writes.
    marker(plan, index)?;
    let entry = &mut plan.entries[index];
    if let Some(record) = &mut entry.presentation {
        let result = tui::reclaim(record);
        util::atomic_write_json(&entry.root.join("presentation.json"), record)?;
        save(env, plan)?;
        result?;
    }
    Ok(())
}

fn require_frontends_gone(entry: &Entry) -> Result<()> {
    for process in &entry.frontends {
        ensure(
            !crate::task_env::observe_process_alive(process)?,
            "closed stage frontend is still running; retain session and clone",
        )?;
    }
    Ok(())
}

fn idle(journal: &J) -> Result<()> {
    if journal["phase"] != "created" {
        return Ok(());
    }
    let socket = upstream::endpoint(&journal["runtime"])?;
    let mut client = crate::codex::rpc::Client::new(&socket.to_string_lossy());
    client.timeout = 5.0;
    let response = client.call(
        "thread/read",
        &json!({"threadId":journal["session"],"includeTurns":false}),
    )?;
    let thread = &response["thread"];
    ensure(
        thread["id"] == journal["session"] && thread["cwd"] == journal["runtime"]["cwd"],
        "stage native thread identity changed",
    )?;
    ensure(
        matches!(
            thread["status"]["type"].as_str(),
            Some("idle" | "notLoaded")
        ) && inactive_flags(&thread["status"]["activeFlags"]),
        "stage session has current native work; retain the original operation",
    )
}

fn inactive_flags(flags: &J) -> bool {
    flags.is_null() || flags.as_array().is_some_and(|flags| flags.is_empty())
}

fn retire_sessions(env: &mut TaskEnvironment, plan: &mut Plan, index: usize) -> Result<()> {
    if plan.entries[index].phase != "sessions" {
        return Ok(());
    }
    if let Some(session) = plan.entries[index]
        .sessions
        .iter()
        .position(|s| s["phase"] != "deleted")
    {
        retire_session(env, plan, index, session)?;
        return pending_step();
    }
    let count = plan.entries[index].sessions.len();
    let descendants = upstream::discover_bounded(
        &plan.entries[index].sessions,
        std::time::Duration::from_secs(5),
    )?;
    ensure(
        count + descendants.len() <= 128,
        "stage descendant inventory exceeds bound",
    )?;
    if !descendants.is_empty() {
        plan.entries[index].sessions.extend(descendants);
        save(env, plan)?;
        return pending_step();
    }
    Ok(())
}

fn retire_session(
    env: &mut TaskEnvironment,
    plan: &mut Plan,
    index: usize,
    session: usize,
) -> Result<()> {
    if plan.entries[index].sessions[session]["phase"] == "created" {
        idle(&plan.entries[index].sessions[session])?;
        plan.entries[index].sessions[session]["phase"] = json!("deleting");
        save(env, plan)?;
    }
    upstream::reclaim_bounded(&mut plan.entries[index].sessions[session], 5.0)?;
    save(env, plan)?;
    if session == 0 {
        util::atomic_write_json(
            &plan.entries[index].root.join("upstream-session.json"),
            &plan.entries[index].sessions[session],
        )?;
    }
    Ok(())
}
