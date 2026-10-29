use super::*;
mod cache;

#[derive(Clone)]
enum Fences {
    Unknown,
    Checkouts(Vec<PathBuf>),
}

impl Fences {
    fn blocks(&self, path: &Path) -> bool {
        match self {
            Self::Unknown => true,
            Self::Checkouts(paths) => paths
                .iter()
                .any(|checkout| crate::task_env::paths_overlap(path, checkout)),
        }
    }
}

pub(crate) fn check_workspace(state: &J, path: &Path) -> Result<()> {
    super::super::coordinator_handoff::check_workspace(state, path)?;
    ensure(
        !fences(state).blocks(path),
        "checkout writer is transferred or draining for human handoff; inspect human-status",
    )
}

fn fences(state: &J) -> Fences {
    let Some(tasks) = state["tasks"].as_object() else {
        return Fences::Unknown;
    };
    let mut paths = Vec::new();
    for (name, task) in tasks {
        match active_record(state, name, task) {
            Ok(Some(record)) => paths.push(PathBuf::from(record["checkout"].as_str().unwrap())),
            Ok(None) => (),
            // An invalid record is a fence, not a transport/metadata failure.
            Err(_) => return Fences::Unknown,
        }
    }
    Fences::Checkouts(paths)
}

fn active_record<'a>(state: &J, name: &str, task: &'a J) -> Result<Option<&'a J>> {
    let record = &task[KEY];
    if record.is_null() {
        return Ok(None);
    }
    let recorded_owner: Owner = serde_json::from_value(record["owner"].clone())?;
    validate(record, &recorded_owner)?;
    // Released history remains valid after its environment mapping retires.
    if record["phase"] == "released" || abandoned_cleanup_complete(task, record) {
        return Ok(None);
    }
    let owner = crate::lifecycle::task_owner(state, name)?
        .ok_or_else(|| FlowError::guard("human writer lost Owner"))?;
    ensure(owner == recorded_owner, "human handoff Owner changed")?;
    Ok(Some(record))
}

fn abandoned_cleanup_complete(task: &J, record: &J) -> bool {
    record["phase"] == "abandoned"
        && task["final_outcome"] == "failure"
        && task["cleanup_obligation"]["status"] == "released"
        && task["cleanup_obligation"]["owner"] == record["owner"]
}

pub(crate) fn admit_workspace(env: &TaskEnvironment, path: &Path) -> Result<()> {
    if let Some(owner) = crate::task_env::isolation::owner(env) {
        let state = queue::load_queue(&owner.queue_root).unwrap_or(J::Null);
        check_workspace(&state, path)?;
    }
    Ok(())
}

pub(crate) fn admit_launch(env: &TaskEnvironment, spec: &J) -> Result<()> {
    admit_workspace(env, Path::new(&required(spec, "cwd")?))
}

pub(crate) fn admit_native(root: &Path, message: &J) -> Result<()> {
    let runtime = read(root, "runtime.json").ok().flatten().unwrap_or(J::Null);
    if super::super::coordinator_handoff::runtime_blocked(root, &runtime).unwrap_or(true) {
        ensure(readonly(message, &runtime), "coordinator handoff refuses new native writes; original approvals and observation remain available")?;
    }
    if !runtime_blocked(root, &runtime).unwrap_or(true) {
        return Ok(());
    }
    ensure(readonly(message, &runtime), "human handoff refuses new native writes; original approvals, interrupts and metadata reads remain available")
}

pub(crate) fn automatic_input_allowed(root: &Path) -> Result<bool> {
    let runtime = read(root, "runtime.json").ok().flatten().unwrap_or(J::Null);
    Ok(!runtime_blocked(root, &runtime).unwrap_or(true)
        && !super::super::coordinator_handoff::runtime_blocked(root, &runtime).unwrap_or(true))
}

fn runtime_blocked(root: &Path, runtime: &J) -> Result<bool> {
    if !runtime.is_object() {
        return Ok(true);
    }
    let Some(owner) = runtime.get("lifecycle_owner").filter(|v| !v.is_null()) else {
        // Only an existing legacy runtime may omit lifecycle ownership.
        return util::protected::is_runtime_namespace(root);
    };
    let owner: Owner = serde_json::from_value(owner.clone())?;
    let path = PathBuf::from(required(runtime, "cwd")?);
    Ok(cache::load(&owner.queue_root)?.blocks(&path))
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
