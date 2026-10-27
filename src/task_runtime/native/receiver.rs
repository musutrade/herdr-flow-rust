//! Fresh coordinator identities, explicit activation/start and bounded recovery.
//! Bootstrap alone never starts a model turn or activates a receiver.
use super::*;
pub mod active;
pub(crate) mod control;
pub mod recovery;

/// Explicit first coordinator turn. Notifications drive later turns. Persist
/// intent before sending so a lost response never starts a duplicate task.
pub fn start(env: &TaskEnvironment, root: &Path, spec: &J) -> Result<J> {
    let _permit = crate::task_env::isolation::mutation_permit(env)?;
    let runtime = started_runtime(env, root)?;
    let owner = crate::task_env::isolation::owner(env)
        .ok_or_else(|| FlowError::guard("coordinator start requires isolated owner"))?;
    let mut result = {
        let _queue_lock = queue::locked(&owner.queue_root, false)?;
        let state = start_state(env, root, &runtime, &owner)?;
        start_reserved(root, &state, spec)?
    };
    result["presentation"] = super::super::tui::after_submission(env, root);
    Ok(result)
}

fn start_state(
    env: &TaskEnvironment,
    root: &Path,
    runtime: &J,
    owner: &crate::lifecycle::Owner,
) -> Result<J> {
    let journal = util::read_state_json(root, "receiver.json")?;
    let guard = PathBuf::from(required(&journal, "guard_dir")?);
    crate::task_env::validate_path(env, &guard)?;
    let state = util::read_state_json(&guard, "state.json")?;
    let q = queue::load_queue(&owner.queue_root)?;
    ensure(
        q["receiver"] == crate::guard::scope(&state) && runtime["instance"] == state["instance"],
        "coordinator registration changed",
    )?;
    Ok(state)
}

fn start_reserved(root: &Path, state: &J, spec: &J) -> Result<J> {
    use crate::herdr::ReceiverAdapter;
    let _lock = crate::guard::locked_existing(root)?;
    ensure_absent(&root.join("receiver-start.json"))?;
    ensure_absent(&root.join("receiver-turn.json"))?;
    let instruction = start_instruction(spec)?;
    let adapter = active::Adapter::new(state)?;
    let mut attempt =
        json!({"phase":"sending","session":state["session"],"instruction":instruction});
    util::atomic_write_json(&root.join("receiver-start.json"), &attempt)?;
    let turn = adapter.prompt(&instruction)?;
    attempt["phase"] = json!("submitted");
    attempt["turn"] = json!(turn);
    util::atomic_write_json(&root.join("receiver-start.json"), &attempt)?;
    Ok(attempt)
}

fn start_instruction(spec: &J) -> Result<String> {
    let instruction = required(spec, "instruction")?;
    ensure(
        instruction.len() <= 65536 && spec.as_object().is_some_and(|o| o.len() == 1),
        "coordinator start accepts only a bounded instruction",
    )?;
    Ok(instruction)
}

pub fn is_task_owned(state: &J) -> bool {
    state["adapter"] == "codex" && state["receiver_kind"] == "task-runtime"
}

pub fn require_adapter(state: &J) -> Result<()> {
    ensure(
        !is_task_owned(state),
        "task-owned Codex receiver is not activated: adapter/watcher unavailable",
    )
}

/// Even a dangling symlink or an unreadable marker reserves the attempt.
pub(in crate::task_runtime) fn ensure_absent(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
        Ok(_) => Err(FlowError::guard(
            "native identity already reserved; inspect without replay",
        )),
    }
}

/// Exactly one thread/start attempt per runtime. A failed/crashed attempt is
/// retained, including failures after the native response or guard publication.
pub fn bootstrap(env: &TaskEnvironment, root: &Path, spec: &J) -> Result<J> {
    let _permit = crate::task_env::isolation::mutation_permit(env)?;
    let runtime = started_runtime(env, root)?;
    let guard = guard_path(env, root, &runtime, spec)?;
    crate::task_env::isolation::runtime_namespace(env, &guard)?;
    let _runtime_lock = crate::guard::locked(root)?;
    let _guard_lock = crate::guard::locked(&guard)?;
    ensure_fresh(root, &guard)?;
    let state = validated_state(root, &guard, &runtime, spec)?;
    reserve_and_create(root, &guard, runtime, state)
}

fn started_runtime(env: &TaskEnvironment, root: &Path) -> Result<J> {
    let runtime = super::super::verify(env, root)?;
    ensure(
        runtime["adapter"] == "codex",
        "receiver bootstrap requires Codex",
    )?;
    ensure(runtime["phase"] == "started", "runtime is not started")?;
    Ok(runtime)
}

fn ensure_fresh(root: &Path, guard: &Path) -> Result<()> {
    for name in ["receiver.json", "creation.json", "state.json"] {
        ensure_absent(&root.join(name))?;
    }
    for name in ["state.json", "receiver.json", "creation.json", "watch.json"] {
        ensure_absent(&guard.join(name))?;
    }
    Ok(())
}

fn validated_state(root: &Path, guard: &Path, runtime: &J, spec: &J) -> Result<J> {
    // Validate known local preconditions before reserving a native attempt.
    client(root)?;
    let state = initial_state(root, guard, runtime, spec);
    approval_policy(&state)?;
    required(&state["model"], "id")?;
    required(&state["model"], "providerID")?;
    Ok(state)
}

fn reserve_and_create(root: &Path, guard: &Path, runtime: J, mut state: J) -> Result<J> {
    let mut journal = json!({"version":1,"phase":"creating","runtime":runtime,
        "guard_dir":guard,"state":state});
    let path = root.join("receiver.json");
    util::atomic_write_json(&path, &journal)?;
    match finish(root, guard, &mut state, &mut journal) {
        Ok(()) => Ok(state),
        Err(error) => {
            journal["phase"] = json!("uncertain");
            journal["state"] = state;
            journal["error_kind"] = json!(error.kind.type_name());
            // Preserve the original failure; the creating marker also blocks replay.
            let _ = util::atomic_write_json(&path, &journal);
            Err(error)
        }
    }
}

fn guard_path(env: &TaskEnvironment, root: &Path, runtime: &J, spec: &J) -> Result<PathBuf> {
    let guard = PathBuf::from(required(spec, "guard_dir")?);
    crate::task_env::validate_path(env, &guard)?;
    let resolved = util::resolve_path(&guard);
    let runtime_root = util::resolve_path(root);
    let cwd = PathBuf::from(required(runtime, "cwd")?);
    crate::task_env::validate_path(env, &cwd)?;
    ensure(
        guard.is_absolute()
            && guard == resolved
            && root == runtime_root
            && guard_inside_runtime(&guard, &runtime_root, &cwd),
        "receiver guard must be a canonical directory below runtime, outside cwd",
    )?;
    Ok(guard)
}

fn guard_inside_runtime(guard: &Path, root: &Path, cwd: &Path) -> bool {
    guard != root
        && util::is_relative_to(guard, root)
        && !crate::task_env::paths_overlap(guard, cwd)
}

fn initial_state(root: &Path, guard: &Path, runtime: &J, spec: &J) -> J {
    json!({"version":2,"adapter":"codex","receiver_kind":"task-runtime",
        "state_dir":guard,"runtime":root,"instance":runtime["instance"],
        "runtime_pid":runtime["pid"],"runtime_start_time":runtime["start_time"],
        "cwd":runtime["cwd"],"model":runtime["model"],
        "approval_policy":runtime.get("approval_policy").cloned().unwrap_or(json!("untrusted")),
        "control_protocol":if super::super::upstream::selected(runtime) {json!("queue-tool-v2")} else {J::Null},
        "task":spec["task"],"role":spec["role"],"auto_rotate":false,"phase":"bootstrap"})
}

fn finish(root: &Path, guard: &Path, state: &mut J, journal: &mut J) -> Result<()> {
    create_codex(root, state)?;
    journal["state"] = state.clone();
    journal["phase"] = json!("created");
    util::atomic_write_json(&root.join("receiver.json"), journal)?;
    crate::guard::save(guard, state)?;
    journal["phase"] = json!("bound");
    util::atomic_write_json(&root.join("receiver.json"), journal)
}
