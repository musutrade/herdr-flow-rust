use super::*;

pub(crate) fn check_workspace(state: &J, path: &Path) -> Result<()> {
    ensure(
        !blocked(state, path)?,
        "checkout writer is transferred or draining for human handoff; inspect human-status",
    )
}

fn blocked(state: &J, path: &Path) -> Result<bool> {
    let Some(tasks) = state["tasks"].as_object() else {
        return Ok(false);
    };
    for (name, task) in tasks {
        let Some(record) = active_record(state, name, task)? else {
            continue;
        };
        let checkout = PathBuf::from(required(record, "checkout")?);
        if crate::task_env::paths_overlap(&path.canonicalize()?, &checkout) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn active_record<'a>(state: &J, name: &str, task: &'a J) -> Result<Option<&'a J>> {
    let record = &task[KEY];
    if record.is_null() {
        return Ok(None);
    }
    let recorded_owner: Owner = serde_json::from_value(record["owner"].clone())?;
    validate(record, &recorded_owner)?;
    // Released history remains valid after its environment mapping retires.
    if record["phase"] == "released" {
        return Ok(None);
    }
    let owner = crate::lifecycle::task_owner(state, name)?
        .ok_or_else(|| FlowError::guard("human writer lost Owner"))?;
    validate(record, &owner)?;
    Ok(Some(record))
}

pub(crate) fn admit_workspace(env: &TaskEnvironment, path: &Path) -> Result<()> {
    if let Some(owner) = crate::task_env::isolation::owner(env) {
        check_workspace(&queue::load_queue(&owner.queue_root)?, path)?;
    }
    Ok(())
}

pub(crate) fn admit_launch(env: &TaskEnvironment, spec: &J) -> Result<()> {
    admit_workspace(env, Path::new(&required(spec, "cwd")?))
}

pub(crate) fn admit_native(root: &Path, message: &J) -> Result<()> {
    let Some(runtime) = read(root, "runtime.json")? else {
        return Ok(());
    };
    if !runtime_blocked(&runtime)? {
        return Ok(());
    }
    ensure(readonly(message, &runtime), "human handoff refuses new native writes; original approvals, interrupts and metadata reads remain available")
}

pub(crate) fn automatic_input_allowed(root: &Path) -> Result<bool> {
    match read(root, "runtime.json")? {
        Some(runtime) => Ok(!runtime_blocked(&runtime)?),
        None => Ok(true),
    }
}

fn runtime_blocked(runtime: &J) -> Result<bool> {
    let Some(owner) = runtime.get("lifecycle_owner").filter(|v| !v.is_null()) else {
        return Ok(false);
    };
    let owner: Owner = serde_json::from_value(owner.clone())?;
    let state = queue::load_queue(&owner.queue_root)?;
    let path = PathBuf::from(required(runtime, "cwd")?);
    blocked(&state, &path)
}

fn readonly(message: &J, runtime: &J) -> bool {
    let Some(method) = message["method"].as_str() else {
        return true;
    };
    if method == "thread/resume" {
        return resume_metadata(&message["params"]);
    }
    matches!(
        method,
        "initialize"
            | "initialized"
            | "thread/read"
            | "thread/list"
            | "thread/loaded/list"
            | "thread/turns/list"
            | "thread/items/list"
            | "thread/backgroundTerminals/list"
            | "thread/goal/get"
            | "model/list"
            | "collaborationMode/list"
            | "hooks/list"
            | "skills/list"
            | "mcpServerStatus/list"
            | "app/list"
            | "app/installed"
            | "app/read"
            | "plugin/list"
            | "config/read"
            | "configRequirements/read"
            | "account/read"
            | "account/rateLimits/read"
            | "experimentalFeature/list"
            | "turn/interrupt"
            | "tool/respond"
    ) || (method == "turn/start" && recovery_turn(&message["params"], runtime))
}

fn resume_metadata(params: &J) -> bool {
    params["threadId"].is_string()
        && params.as_object().is_some_and(|fields| {
            fields.iter().all(|(key, value)| {
                match key.as_str() {
                    "threadId" => true,
                    "excludeTurns" => value.is_boolean(),
                    "initialTurnsPage" => value.is_null() || value.is_object(),
                    // Native PreserveExistingThread serializes these absent overrides
                    // as null. serviceTier is deliberately excluded: null clears it.
                    "history"
                    | "path"
                    | "model"
                    | "modelProvider"
                    | "cwd"
                    | "runtimeWorkspaceRoots"
                    | "approvalPolicy"
                    | "approvalsReviewer"
                    | "sandbox"
                    | "permissions"
                    | "config"
                    | "baseInstructions"
                    | "developerInstructions"
                    | "personality" => value.is_null(),
                    _ => false,
                }
            })
        })
}

fn recovery_turn(params: &J, runtime: &J) -> bool {
    runtime["recovery_role"] == true
        && params["sandboxPolicy"] == json!({"type":"readOnly","networkAccess":false})
        && params["approvalPolicy"] == "never"
        && params.as_object().is_some_and(|p| {
            p.keys().all(|k| {
                [
                    "threadId",
                    "input",
                    "model",
                    "effort",
                    "sandboxPolicy",
                    "approvalPolicy",
                    "outputSchema",
                ]
                .contains(&k.as_str())
            })
        })
}
